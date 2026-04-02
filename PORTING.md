# Porting Notes: cosmic-comp Fork

This document describes all changes made to the cosmic-comp upstream codebase,
the rationale behind each, and the process for keeping the fork in sync with upstream.

## Upstream

- **Source:** https://github.com/pop-os/cosmic-comp
- **Tracked as:** `git remote add upstream https://github.com/pop-os/cosmic-comp`
- **Branch strategy:** `master` tracks upstream directly; our changes sit on top as a
  series of commits. Run `git log --oneline upstream/master..HEAD` to see the full
  diff against upstream at any time.

## What we keep

Everything related to the Wayland compositor core:

- `src/backend/` — DRM/KMS, rendering, Winit and X11 backends
- `src/shell/layout/` — floating and tiling layout engine (the primary reason for this fork)
- `src/shell/element/` — window elements, stack, tabs (retains cosmic/Iced dependency for now, see Technical Debt below)
- `src/shell/workspace.rs` — workspace management
- `src/shell/focus/` — keyboard focus management
- `src/input/` — input handling, gestures
- `src/wayland/handlers/` — standard Wayland protocol handlers
- `src/xwayland.rs` — XWayland integration

## What we add

### `src/event_bus.rs`

Non-blocking Event Bus integration. Opens a Unix socket connection to the Lunaris
Event Bus in a background thread and emits structured events without touching the
compositor render loop.

**Events emitted:**

| Event type | Trigger | Source field |
|---|---|---|
| `window.opened` | Surface successfully mapped to workspace | `wayland` |
| `window.focused` | Keyboard focus changes | `wayland` |
| `window.closed` | Toplevel surface destroyed | `wayland` |
| `clipboard.copy` | Selection changes on clipboard target | `wayland` |

The socket path is read from `LUNARIS_PRODUCER_SOCKET` (default:
`/run/lunaris/event-bus-producer.sock`). The session ID is read from
`LUNARIS_SESSION_ID` (default: a fresh UUID v7 generated at startup).

**Design constraint:** All Event Bus calls are non-blocking. The compositor event
loop must never stall waiting for I/O. The background thread absorbs all socket
writes; if the channel is full, events are dropped with a warning rather than
blocking.

### Changes to `src/state.rs`

Added `event_bus: crate::event_bus::EventBusHandle` field to `Common`. Initialized
in the `Common` constructor via `crate::event_bus::spawn()`.

### Changes to `src/shell/focus/mod.rs`

Added `emit_window_focused` call in `Shell::set_focus` after the focus stack is
updated, before `update_focus_state`.

### Changes to `src/wayland/handlers/compositor.rs`

Added `emit_window_opened` call after successful `shell.map_window` in the Wayland
compositor handler.

### Changes to `src/xwayland.rs`

Added `emit_window_opened` call after successful `shell.map_window` in the XWayland
map notify handler.

### Changes to `src/wayland/handlers/xdg_shell/mod.rs`

Added `emit_window_closed` call at the start of `toplevel_destroyed`, before the
surface is unmapped (so the app ID is still available).

### Changes to `src/wayland/handlers/selection.rs`

Added `emit_clipboard_copy` call at the start of `new_selection` for
`SelectionTarget::Clipboard`. The MIME type is included; clipboard content is never
logged.

### Changes to `Cargo.toml`

Added `prost` and `uuid` as dependencies, `prost-build` as a build dependency.

### Changes to `build.rs`

Added `prost_build::compile_protos` call to generate Rust bindings from
`proto/event.proto`.

### New file: `proto/event.proto`

Copy of the Event Bus protobuf schema from the `event-bus` repository. Must be kept
in sync with `event-bus/proto/event.proto`. A shared `proto` crate is planned for
Phase 2 to eliminate this duplication.

### Exclusive zone fix for 4-anchor layer surfaces

`src/wayland/handlers/compositor.rs` -- Before `layer_map_for_output().arrange()` is
called, we inject `exclusive_edge = Some(Anchor::TOP)` into the cached state of any
layer surface that has all 4 anchors set and an exclusive zone but no explicit edge.
Smithay's `implied_exclusive_edge_for_anchor()` returns `None` for 4-anchor surfaces
(by design: it cannot infer which edge to reserve), which silently disables the
exclusive zone. The desktop-shell uses all 4 anchors for its full-screen overlay
surface with a 36px top bar. Without this fix, windows slide under the bar.

### lunaris-shell-overlay protocol: tab bar and indicator events

`resources/protocols/lunaris-shell-overlay.xml` -- Extended with tab bar events
(`tab_bar_show`, `tab_bar_hide`, `tab_added`, `tab_removed`, `tab_activated`,
`tab_title_changed`, `tab_activate` request), indicator events (`indicator_show`,
`indicator_hide` with `indicator_kind` enum for stack hover, swap, and resize), and
supporting enums. The protocol allows the compositor to delegate rendering of window
stack tab bars and visual indicators to the desktop-shell process.

`src/wayland/protocols/shell_overlay.rs` -- Added `send_tab_*` and
`send_indicator_show/hide` methods on `ShellOverlayState`, plus the `tab_activate`
handler trait method and dispatch match arm.

`src/wayland/handlers/shell_overlay.rs` -- Implemented `tab_activate`: finds the
stack by `stack_id`, calls `set_active()` on the matching surface.

`src/shell/element/stack.rs` -- Added `stack_id: u32` field with static atomic
counter to `CosmicStackInternal`. Protocol events are emitted from `add_window()`,
`remove_window()`, `remove_idx()`, and `set_active()` via `loop_handle.insert_idle`.
Initial tab list is sent from `CosmicStack::new()`.

`src/wayland/handlers/compositor.rs` -- Added title change detection in the commit
handler: compares current title against a `CachedTitle` in the surface's user data
and emits `tab_title_changed` when it differs.

`src/shell/mod.rs` -- Indicator show events in `set_overview_mode()` (swap,
kind=2) and `set_resize_mode()` (resize, kind=3). Hide events via
`pending_indicator_hides` drained in `Common::refresh()`.

`src/shell/grabs/moving.rs` -- Stack hover indicator show/hide in the move grab
motion handler and both `unset()` implementations.

`src/shell/layout/tiling/mod.rs` -- Resize edge change events emitted when
`possible_edges` differs from the cached value.

### Iced rendering removal (Phase 3 partial)

`src/shell/element/resize_indicator.rs`, `stack_hover.rs`, `swap_indicator.rs` --
`view()` returns an empty row. All Iced widget imports removed. The underlying
`IcedElement` wrapper and `Program` trait implementation remain because they provide
the `SpaceElement` integration, input handling, and render pipeline that the
compositor relies on. Rendering is now delegated to desktop-shell via the indicator
protocol events above.

`src/shell/element/stack.rs` -- `DefaultDecorations::view()` replaced with a minimal
mouse area that preserves `DragStart` and `Menu` messages without rendering any tab
widgets. `TabMessage` trait, scroll-related message variants, and the `SCROLLABLE_ID`
static removed.

`src/shell/element/stack/tab.rs` and `tabs.rs` -- Deleted. These were the custom
Iced `Widget` implementations for individual tabs and the scrollable tab list. Tab
rendering is now handled by desktop-shell via the tab bar protocol.

The menu Iced fallback path (`src/shell/grabs/menu/`) is intentionally kept because
zoom.rs context menus still use it.

### Config: TOML loader replaces cosmic-config for CosmicComp

`src/config/mod.rs` -- The `com.system76.CosmicComp` cosmic-config store and its
`ConfigWatchSource` are replaced by a TOML file at `~/.config/lunaris/compositor.toml`
(configurable via `LUNARIS_COMPOSITOR_CONFIG`). A `notify` file watcher on the parent
directory detects changes (including atomic editor renames) and sends them to the
calloop event loop via a channel. The `toml_config_changed()` function reloads the
entire TOML file, compares each field against the previous config, and triggers the
same side effects as the old key-based `config_changed()` handler.

The `cosmic_helper` field on `Config` is retained as a legacy write-back channel
because `zoom.rs` and `input/actions.rs` write config values back to cosmic-config.
This will be removed when zoom.rs is replaced.

CosmicTk, Shortcuts, and WindowRules watchers remain on cosmic-config unchanged.

`cosmic-comp-config/src/lib.rs` -- Added `#[derive(Deserialize, Serialize)]` to
`CosmicCompConfig` so it can be deserialized from TOML directly. The
`CosmicConfigEntry` derive is kept for backward compatibility with the legacy
cosmic-config code paths.

`Cargo.toml` -- Added `toml = "0.8"` and `notify = "7"` dependencies.

### Sandbox identifier change

`src/state.rs` -- `ClientState::not_sandboxed()` previously checked for
`com.system76.CosmicPanel` as the sandbox engine. Changed to
`dev.lunaris.desktop-shell` to match the Lunaris desktop shell identity.

## Technical debt

### cosmic/Iced: remaining usage

The `cosmic` and `iced` crates remain as dependencies for three reasons:

1. **zoom.rs** -- The accessibility zoom toolbar is a fully interactive Iced widget
   with 7 buttons and a context menu. It has not been replaced by a protocol event.
   This is the last Iced UI element that renders visible content.

2. **utils/iced/mod.rs + state.rs** -- The `IcedElement<P>` wrapper provides the
   `SpaceElement`, `PointerTarget`, `KeyboardTarget`, and `TouchTarget`
   implementations for `CosmicStack`. Even though `view()` returns empty content,
   the wrapper is needed for input routing (DragStart, Menu) and the Smithay
   render pipeline integration. Replacing it requires a custom `SpaceElement` for
   stacks.

3. **cosmic::Theme** -- Used throughout the compositor for color access
   (`accent_color()`, `primary_container_color()`, palette lookups). The theme
   watcher in `theme.rs` depends on `cosmic_config`. Replacing this requires a
   Lunaris-native theme system that reads from `theme.toml`.

4. **menu/mod.rs fallback** -- The Iced context menu rendering path is kept for
   zoom.rs menus and as a fallback when no desktop-shell client is connected.

## Upstream sync process

### Weekly rebase (automated)

A scheduled CI job runs weekly:

1. `git fetch upstream`
2. Attempts `git rebase upstream/master` on a test branch
3. If successful, opens a pull request for review
4. If conflicts occur, opens a GitHub issue listing the conflicting files

### Manual rebase

```bash
git fetch upstream
git rebase upstream/master
# Resolve conflicts if any, then:
git rebase --continue
```

### Contributing patches upstream

Any change that is not Lunaris-specific should be submitted as a pull request to
cosmic-comp upstream. A shorter patch series means less rebase work on every
upstream update. Check before committing whether a change belongs upstream.

### Conflict resolution guidelines

**Conflicts in `src/backend/`:** Upstream changes take priority unless they conflict
with our Event Bus socket setup. Accept upstream.

**Conflicts in `src/shell/layout/`:** Upstream changes take priority. This is the
core we fork for; keep it as close to upstream as possible.

**Conflicts in `src/shell/focus/mod.rs`:** Our `emit_window_focused` call must be
preserved after the focus stack update. Re-apply it after accepting upstream changes.

**Conflicts in `src/wayland/handlers/`:** Review case by case. Our additions are
minimal (single function calls); they are easy to re-apply after upstream changes.

**Conflicts in `src/event_bus.rs`:** This file is entirely ours; upstream will never
touch it. No conflicts expected.
