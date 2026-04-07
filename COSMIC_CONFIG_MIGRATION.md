# cosmic-config Migration Status

## Already Done

The main compositor config (`CosmicCompConfig`) already loads from `~/.config/lunaris/compositor.toml` with notify-based hot reload. This covers:

- XKB keyboard layout (layout, model, variant, options, repeat rate/delay)
- Input settings (natural scroll, tap-to-click, accel profile, scroll method)
- Workspace settings (count, layout Horizontal/Vertical)
- Autotile behavior
- Focus follows cursor / cursor follows focus
- Active hint settings
- Accessibility zoom

These settings are fully migrated. The TOML loading happens in `src/config/mod.rs` via `load_toml_config()` with file watching via `notify`.

## Remaining cosmic-config Usage (3 areas)

### 1. Shortcuts / Keybindings

**Location:** `src/config/mod.rs` lines 257-292

Uses `cosmic_settings_config::Shortcuts` and `cosmic_config::calloop::ConfigWatchSource` to load keybindings from `com.system76.CosmicSettings.Shortcuts`.

**Migration path:**
- Create `~/.config/lunaris/keybindings.toml` with `[compositor]` section
- Parse with `lunaris-config::keybindings::parse_keybindings()`
- Replace `Shortcuts` type with Lunaris `KeybindingConfig`
- Replace `ConfigWatchSource` with notify file watcher
- Map Lunaris `Action` to existing compositor action dispatch

**Blockers:**
- Need to define Lunaris keybinding action types that cover all current cosmic shortcuts
- The `shortcuts::action::System` enum has ~30 actions (lock, suspend, screenshot, etc.)
- Existing code uses `shortcuts::action::Direction`, `shortcuts::action::ResizeDirection` etc.

### 2. Window Rules / Tiling Exceptions

**Location:** `src/config/mod.rs` lines 294-324

Uses `cosmic_settings_config::window_rules` to load tiling exceptions.

**Migration path:**
- Add `[tiling_exceptions]` to `compositor.toml`
- Define Lunaris `TilingException` struct (app_id pattern, exception type)
- Replace `ApplicationException` with Lunaris type

**Blockers:**
- Simple struct, relatively easy to migrate
- Need to define exception format in TOML

### 3. Zoom Write-back

**Location:** `src/config/mod.rs` line 338-340

Legacy `cosmic_config::Config::new("com.system76.CosmicComp", 1)` used by `zoom.rs` to persist zoom state.

**Migration path:**
- Write zoom state to a Lunaris state file instead
- Use `src/config/dynamic.rs` pattern (already saves outputs/numlock state)

**Blockers:** None, straightforward.

## Dependencies to Remove

When all three areas are migrated:

```toml
# Remove from Cargo.toml:
cosmic-config = { ... }

# Remove from cosmic-comp-config/Cargo.toml:
cosmic-config = { ... }
```

Also remove:
- `CosmicConfigEntry` derive from `cosmic-comp-config/src/lib.rs`
- `cosmic_config::CosmicConfigEntry` import from `src/backend/mod.rs`
- `cosmic_config::ConfigSet` imports from `src/shell/mod.rs` and `src/input/actions.rs`

## Files Affected

| File | cosmic-config Usage | Migration Difficulty |
|------|-------------------|---------------------|
| `src/config/mod.rs` | Shortcuts watch, WindowRules watch, cosmic_helper | Medium |
| `cosmic-comp-config/src/lib.rs` | CosmicConfigEntry derive | Easy (remove derive) |
| `cosmic-comp-config/Cargo.toml` | cosmic-config dependency | Easy |
| `Cargo.toml` | cosmic-config dependency | Easy |
| `src/backend/mod.rs` | CosmicConfigEntry import | Easy |
| `src/shell/mod.rs` | ConfigSet import | Easy |
| `src/input/actions.rs` | ConfigSet import | Easy |
| `src/input/mod.rs` | Uses Shortcuts type for keybinding dispatch | Hard |
