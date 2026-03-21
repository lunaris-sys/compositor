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

## Technical debt

### cosmic/Iced window decorations

`src/shell/element/` and `src/shell/grabs/` use the `cosmic` crate (built on Iced)
for window decorations: header bars, stack tabs, resize indicators, and context
menus. This dependency is retained for Phase 1B because removing it would require
reimplementing all window decorations before the shell infrastructure exists.

Window decorations will be reimplemented using the Lunaris shell (Tauri +
TypeScript) in Phase 2. See issue #11 in this repository for the full scope.

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
