# compositor

Lunaris compositor is a fork of [cosmic-comp](https://github.com/pop-os/cosmic-comp), the Wayland compositor from System76's COSMIC desktop. It adds an Event Bus integration layer that emits structured events for window and clipboard activity.

This repo is not a GitHub fork. It is a standalone repo with cosmic-comp tracked as a git remote so we can pull upstream changes without being coupled to their branching model.

## What's changed from upstream

All changes are documented in [PORTING.md](PORTING.md). The short version:

- **`src/event_bus.rs`** — new file. Background thread that connects to the Lunaris Event Bus and emits events. Non-blocking: if the Event Bus is not running, events are dropped silently.
- **`src/state.rs`** — `EventBusHandle` added to `Common`
- **`src/shell/focus/mod.rs`** — emits `window.focused`
- **`src/wayland/handlers/compositor.rs`** — emits `window.opened` after map
- **`src/xwayland.rs`** — emits `window.opened` for XWayland windows
- **`src/wayland/handlers/xdg_shell/mod.rs`** — emits `window.closed` on destroy
- **`src/wayland/handlers/selection.rs`** — emits `clipboard.copy` on clipboard write

## Events emitted

| Event type | When | Payload |
|---|---|---|
| `window.opened` | Window maps (becomes visible) | `app_id`, `title` |
| `window.focused` | Input focus changes | `app_id`, `title` |
| `window.closed` | Window destroys | `app_id` |
| `clipboard.copy` | Clipboard content changes | none (content is not captured) |

## Configuration

| Variable | Default | Description |
|---|---|---|
| `LUNARIS_PRODUCER_SOCKET` | `/run/lunaris/event-bus-producer.sock` | Event Bus producer socket |
| `LUNARIS_SESSION_ID` | `unknown` | Session ID attached to all events |

## Staying in sync with upstream

A weekly CI workflow (`.github/workflows/upstream-rebase.yml`) rebases against `upstream/master`. On success it opens a PR. On conflict it opens an issue with label `upstream-rebase-conflict`.

To rebase manually:
```bash
git fetch upstream
git rebase upstream/master
# resolve conflicts if any, then push
```

## Development

Window decorations are currently handled by cosmic-comp's built-in Iced/cosmic implementation. Replacing them with Lunaris CSD via ui-kit is tracked in issue #11 and planned for Phase 2B.

## Testing

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Integration tests require a running X11/Wayland session and are located in `distro/tests/integration_compositor.rs`.

## Part of

[Lunaris](https://github.com/lunaris-sys) — a Linux desktop OS built around a system-wide knowledge graph.
