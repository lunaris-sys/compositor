// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    shell::Shell,
    state::{BackendData, State},
    utils::prelude::OutputExt,
    wayland::protocols::{
        output_configuration::OutputConfigurationState, workspace::WorkspaceUpdateGuard,
    },
};
use anyhow::Context;
// cosmic_config is still needed for Shortcuts, WindowRules,
// and the legacy cosmic_helper write-back used by zoom.rs.
use cosmic_settings_config::window_rules::ApplicationException;
use cosmic_settings_config::{Shortcuts, shortcuts, window_rules};
use serde::{Deserialize, Serialize};
use smithay::utils::{Clock, Monotonic};
use smithay::wayland::xdg_activation::XdgActivationState;
pub use smithay::{
    backend::input::{self as smithay_input, KeyState},
    input::keyboard::{Keysym, ModifiersState, keysyms as KeySyms},
    output::{Mode, Output},
    reexports::{
        calloop::LoopHandle,
        input::{
            AccelProfile, ClickMethod, Device as InputDevice, ScrollMethod, SendEventsMode,
            TapButtonMap,
        },
    },
    utils::{Logical, Physical, Point, SERIAL_COUNTER, Size, Transform},
};
use std::{
    cell::{Ref, RefCell},
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use tracing::{error, warn};

pub mod appearance;
mod input_config;
pub mod key_bindings;
mod types;

pub use cosmic_comp_config::EdidProduct;
use cosmic_comp_config::{
    CosmicCompConfig, XkbConfig,
    input::{
        AccelConfig, DeviceState as InputDeviceState, InputConfig, ScrollConfig, TapConfig,
        TouchpadOverride,
    },
    output::comp::{OutputConfig, OutputInfo, OutputState, OutputsConfig, TransformDef},
};
pub use key_bindings::{Action, PrivateAction, action_from_str, keysym_from_str};
use types::WlXkbConfig;

#[derive(Debug)]
pub struct Config {
    pub dynamic_conf: DynamicConfig,
    /// Path to the Lunaris TOML compositor config file.
    pub toml_path: PathBuf,
    /// Compositor configuration loaded from TOML.
    pub cosmic_conf: CosmicCompConfig,
    /// Cosmic-shape `Shortcuts` table. Populated from
    /// `toml_keybindings` at load time (CC3) — the
    /// `cosmic-settings-daemon` source is no longer consulted.
    /// Kept in cosmic shape because the input-dispatch paths in
    /// `input/mod.rs`, `shell/element/resize_indicator.rs`, the
    /// tiling-swap grab, and the resize-mode arrow handler all
    /// iterate it as `Vec<(Binding, Action)>`.
    pub shortcuts: Shortcuts,
    /// Tiling exceptions. Historically populated from
    /// `com.system76.CosmicSettings.WindowRules`; that source was
    /// dropped in compositor #29 / CC3. The field remains as an
    /// always-empty `Vec` so call sites that already iterate over
    /// it (`shell/mod.rs::TilingExceptions::new`) compile without
    /// change. Lunaris-side window-rule semantics live in
    /// `layout.window_rules`.
    pub tiling_exceptions: Vec<ApplicationException>,
    /// Layout and tiling configuration from TOML.
    pub layout: LayoutConfig,
    /// Keybindings from TOML `[keybindings]` section.
    pub toml_keybindings: Vec<KeyBinding>,
    /// System actions sourced from `[system_actions]` in
    /// `compositor.toml`, layered on top of
    /// `default_system_actions()`.
    pub system_actions: BTreeMap<shortcuts::action::System, String>,
    /// True when running nested inside another Wayland compositor (Winit/X11 backend).
    /// Note: The XKB layout is applied normally even in nested mode because
    /// the compositor receives scancodes (not keysyms) from the host.
    pub nested: bool,
}

#[derive(Debug)]
pub struct DynamicConfig {
    outputs: (Option<PathBuf>, OutputsConfig),
    numlock: (Option<PathBuf>, NumlockStateConfig),
    accessibility_filter: (Option<PathBuf>, ScreenFilter),
    /// Runtime-mutable compositor state (autotile toggle,
    /// pinned-workspace list). Persisted to TOML at
    /// `~/.local/state/lunaris/compositor/state.toml` so the
    /// values survive a restart. Read on session start, written
    /// through `runtime_state_mut()` whenever a user action
    /// changes them.
    ///
    /// The previous home for these was `cosmic_helper.set("autotile", _)`
    /// and `set("pinned_workspaces", _)` — the cosmic-config
    /// writeback we are removing as part of compositor #29.
    runtime_state: (Option<PathBuf>, LunarisRuntimeState),
}

#[derive(Default, Debug, Deserialize, Serialize)]
pub struct NumlockStateConfig {
    pub last_state: bool,
}

/// Runtime-mutable compositor state held in a single state file.
///
/// Distinct from `compositor.toml`: that file is user-authored
/// configuration, this one is runtime memory of the last toggle
/// state and the pinned-workspace list. Lives under XDG state, not
/// XDG config, per the file-system-hierarchy convention.
///
/// Fields are `Option` so the precedence rule is unambiguous:
/// `Some(_)` means "the user has explicitly persisted this value
/// since startup, override compositor.toml with it"; `None` means
/// "no override, use whatever compositor.toml says". A missing or
/// freshly-defaulted state file therefore cannot silently revert
/// the user's TOML — fix for compositor #29 review HIGH 2.
#[derive(Default, Debug, Deserialize, Serialize, Clone)]
pub struct LunarisRuntimeState {
    /// Persisted autotile toggle. `None` ⇒ use TOML's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autotile: Option<bool>,
    /// Persisted pinned-workspace list. `None` ⇒ use TOML's value.
    /// The shape is the cosmic-comp `PinnedWorkspace` type from the
    /// local `cosmic-comp-config` crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_workspaces:
        Option<Vec<cosmic_comp_config::workspace::PinnedWorkspace>>,
}

pub struct CompOutputConfig<'a>(pub Ref<'a, OutputConfig>);

impl CompOutputConfig<'_> {
    pub fn mode_size(&self) -> Size<i32, Physical> {
        self.0.mode.0.into()
    }

    pub fn mode_refresh(&self) -> u32 {
        self.0.mode.1.unwrap_or(60_000)
    }

    pub fn transformed_size(&self) -> Size<i32, Physical> {
        self.transform().transform_size(self.mode_size())
    }

    pub fn output_mode(&self) -> Mode {
        Mode {
            size: self.mode_size(),
            refresh: self.mode_refresh() as i32,
        }
    }

    pub fn transform(&self) -> Transform {
        Transform::from(CompTransformDef(self.0.transform))
    }
}

pub struct CompTransformDef(pub TransformDef);

impl From<Transform> for CompTransformDef {
    fn from(transform: Transform) -> Self {
        let def = match transform {
            Transform::Normal => TransformDef::Normal,
            Transform::_90 => TransformDef::_90,
            Transform::_180 => TransformDef::_180,
            Transform::_270 => TransformDef::_270,
            Transform::Flipped => TransformDef::Flipped,
            Transform::Flipped90 => TransformDef::Flipped90,
            Transform::Flipped180 => TransformDef::Flipped180,
            Transform::Flipped270 => TransformDef::Flipped270,
        };
        CompTransformDef(def)
    }
}

impl From<CompTransformDef> for Transform {
    fn from(comp_transform: CompTransformDef) -> Self {
        match comp_transform.0 {
            TransformDef::Normal => Transform::Normal,
            TransformDef::_90 => Transform::_90,
            TransformDef::_180 => Transform::_180,
            TransformDef::_270 => Transform::_270,
            TransformDef::Flipped => Transform::Flipped,
            TransformDef::Flipped90 => Transform::Flipped90,
            TransformDef::Flipped180 => Transform::Flipped180,
            TransformDef::Flipped270 => Transform::Flipped270,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
pub struct ScreenFilter {
    pub inverted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_filter: Option<ColorFilter>,
    /// Night-light multiplicative tint as `(r, g, b)` ratios in
    /// `[0.0, 1.0]`. `None` means "no tint" (= identity / 6500K).
    /// Applied as a final multiply in `offscreen.frag` so it works
    /// on every backend, not just KMS hardware-gamma. Skipped from
    /// serialization because this is computed live from the night-
    /// light state, not a persisted preference.
    #[serde(skip)]
    pub night_light_tint: Option<[f32; 3]>,
}

impl ScreenFilter {
    pub fn is_noop(&self) -> bool {
        !self.inverted && self.color_filter.is_none() && self.night_light_tint.is_none()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
// these values need to match with offscreen.frag
pub enum ColorFilter {
    Greyscale = 1,
    Protanopia = 2,
    Deuteranopia = 3,
    Tritanopia = 4,
}

// ── Layout configuration ─────────────────────────────────────────────────────

/// Layout and tiling configuration loaded from `[layout]` in compositor.toml.
#[derive(Debug, Clone)]
pub struct LayoutConfig {
    /// Inner gap between tiled windows (pixels).
    pub inner_gap: i32,
    /// Outer gap between tiled windows and screen edges (pixels).
    pub outer_gap: i32,
    /// When true, no gaps are applied when a workspace has only one tiled window.
    pub smart_gaps: bool,
    /// Render Lunaris window-control headers on **single tiled** SSD
    /// windows. Default `false` to match tiling-WM convention
    /// (i3, sway, hyprland) — tiled chrome is dead pixels in a
    /// keyboard-driven layout. Stacks always keep their tab-bar
    /// header regardless of this setting (the tab-bar is
    /// functional UI for window-switching, not pure decoration).
    /// Floating windows always keep their headers regardless.
    pub tiled_headers: bool,
    /// Window rules for float/tile decisions.
    pub window_rules: Vec<WindowRule>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            inner_gap: 8,
            outer_gap: 8,
            smart_gaps: true,
            tiled_headers: false,
            window_rules: Vec::new(),
        }
    }
}

/// A rule that determines whether a window should float or tile.
#[derive(Debug, Clone)]
pub struct WindowRule {
    pub matcher: WindowMatch,
    pub action: WindowAction,
}

/// Matching criteria for a window rule.
#[derive(Debug, Clone)]
pub struct WindowMatch {
    /// Regex pattern for the app_id. None = match any.
    pub app_id: Option<regex::Regex>,
    /// Regex pattern for the window title. None = match any.
    pub title: Option<regex::Regex>,
    /// Match on window type (e.g. "dialog").
    pub window_type: Option<String>,
}

/// What to do with a matched window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Float,
    Tile,
}

impl WindowMatch {
    /// Check whether a window matches this rule.
    pub fn matches(&self, app_id: &str, title: &str, is_dialog: bool) -> bool {
        if let Some(ref wt) = self.window_type {
            if wt == "dialog" && !is_dialog {
                return false;
            }
        }
        if let Some(ref re) = self.app_id {
            if !re.is_match(app_id) {
                return false;
            }
        }
        if let Some(ref re) = self.title {
            if !re.is_match(title) {
                return false;
            }
        }
        true
    }
}

// ── Keybinding configuration ─────────────────────────────────────────────────

/// A parsed keybinding: modifier set + key -> action string.
#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub modifiers: KeyBindingModifiers,
    pub key: String,
    pub action: String,
}

/// Modifier flags for a keybinding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyBindingModifiers {
    pub super_key: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Parse a keybinding string like "Super+Shift+H" into modifiers + key.
///
/// Exposed so the dynamic binding resolver can re-use the same grammar
/// for D-Bus-registered bindings without duplicating parsing logic.
pub fn parse_keybinding(binding: &str) -> Option<(KeyBindingModifiers, String)> {
    let mut mods = KeyBindingModifiers::default();
    let parts: Vec<&str> = binding.split('+').collect();
    if parts.is_empty() {
        return None;
    }
    for part in &parts[..parts.len() - 1] {
        match part.to_lowercase().as_str() {
            "super" | "logo" | "mod4" => mods.super_key = true,
            "shift" => mods.shift = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "mod1" => mods.alt = true,
            _ => {}
        }
    }
    let key = parts.last()?.to_string();
    Some((mods, key))
}

/// Default TOML config path.
const DEFAULT_TOML_PATH: &str = ".config/lunaris/compositor.toml";

/// Display profile config (output management) lives in a dedicated
/// file under `compositor.d/` so the compositor can rewrite it on
/// every applied output change without disturbing the user-edited
/// `compositor.toml`. See `docs/architecture/display-system.md` §A1.
const DISPLAYS_TOML_PATH: &str = ".config/lunaris/compositor.d/displays.toml";

/// Resolve the absolute path of the displays-config TOML, or `None`
/// if `$HOME` is unset (which only happens in degenerate test rigs).
pub fn displays_toml_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        let mut p = std::path::PathBuf::from(home);
        p.push(DISPLAYS_TOML_PATH);
        p
    })
}

/// Drop-in directory for keybinding fragments written by `installd`
/// on module install. One `*.toml` file per module, each a flat
/// `[keybindings]` table. Loaded alongside the main compositor.toml
/// and fed into the binding resolver at `BindingScope::Module`.
const KEYBINDINGS_FRAGMENT_DIR: &str = "compositor.d/keybindings.d";

/// Return the absolute path of the keybinding fragment directory for
/// the given main `compositor.toml` path.
pub fn keybinding_fragment_dir(toml_path: &std::path::Path) -> std::path::PathBuf {
    toml_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(KEYBINDINGS_FRAGMENT_DIR)
}

/// Parsed entry from a keybinding fragment file. `module_id` is the
/// file stem, so two modules can never collide (fs atomicity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentEntry {
    pub module_id: String,
    pub binding: String,
    pub action: String,
}

/// Scan `dir` and return every `"accelerator" = "action"` pair from
/// every `*.toml` file. Missing / malformed files are skipped with a
/// warning — a broken fragment must not crash the compositor.
pub fn load_keybinding_fragments(dir: &std::path::Path) -> Vec<FragmentEntry> {
    if !dir.exists() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                "keybinding fragments: cannot read {}: {err}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let module_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("keybinding fragments: read {} failed: {err}", path.display());
                continue;
            }
        };
        let table: toml::Table = match toml::from_str(&content) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!("keybinding fragments: parse {} failed: {err}", path.display());
                continue;
            }
        };
        let Some(kb_table) = table.get("keybindings").and_then(|v| v.as_table()) else {
            continue;
        };
        for (binding, action) in kb_table {
            let Some(action_str) = action.as_str() else { continue };
            out.push(FragmentEntry {
                module_id: module_id.clone(),
                binding: binding.clone(),
                action: action_str.to_string(),
            });
        }
    }
    tracing::info!(
        "keybinding fragments: loaded {} binding(s) from {}",
        out.len(),
        dir.display(),
    );
    out
}

/// Load CosmicCompConfig from a TOML file, falling back to defaults.
///
/// The user TOML is typically a sparse file with only the fields the
/// user wants to override. We start from defaults and apply overrides
/// for the sections we recognize.
/// Parsed result from the TOML compositor config.
struct TomlConfig {
    cosmic: CosmicCompConfig,
    layout: LayoutConfig,
    keybindings: Vec<KeyBinding>,
    /// User overrides for the system-action → spawn-command map.
    /// Merged on top of `default_system_actions()` at config-load
    /// time. Empty when no `[system_actions]` section is present
    /// in compositor.toml.
    system_actions: BTreeMap<shortcuts::action::System, String>,
}

fn load_toml_config(path: &std::path::Path) -> TomlConfig {
    let default = || TomlConfig {
        cosmic: CosmicCompConfig::default(),
        layout: LayoutConfig::default(),
        keybindings: Vec::new(),
        system_actions: BTreeMap::new(),
    };

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            tracing::info!(
                "no compositor.toml at {}, using defaults",
                path.display()
            );
            return default();
        }
    };

    let table: toml::Table = match toml::from_str(&contents) {
        Ok(t) => t,
        Err(err) => {
            warn!(?err, "failed to parse compositor.toml");
            return default();
        }
    };

    let mut config = CosmicCompConfig::default();

    // Apply xkb_config overrides.
    //
    // XKB natively accepts comma-separated layouts and variants in a
    // single string (`"de,us"` + options like `grp:alt_shift_toggle`).
    // We let users write the friendlier TOML `layouts = ["de", "us"]`
    // form too, and fold both into the upstream single-string field
    // `cosmic_comp_config::XkbConfig::layout`. Single-scalar `layout =
    // "de"` keeps working for back-compat; the array form wins when
    // both are present.
    if let Some(xkb) = table.get("xkb_config").and_then(|v| v.as_table()) {
        if let Some(list) = xkb.get("layouts").and_then(|v| v.as_array()) {
            let joined = list
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(",");
            if !joined.is_empty() {
                config.xkb_config.layout = joined;
            }
        } else if let Some(s) = xkb.get("layout").and_then(|v| v.as_str()) {
            config.xkb_config.layout = s.to_string();
        }
        if let Some(s) = xkb.get("model").and_then(|v| v.as_str()) {
            config.xkb_config.model = s.to_string();
        }
        if let Some(list) = xkb.get("variants").and_then(|v| v.as_array()) {
            let joined = list
                .iter()
                .map(|v| v.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(",");
            // Trailing empty entries (e.g. `["","dvorak"]` → `",dvorak"`)
            // are legitimate — XKB uses position to pair with layouts.
            config.xkb_config.variant = joined;
        } else if let Some(s) = xkb.get("variant").and_then(|v| v.as_str()) {
            config.xkb_config.variant = s.to_string();
        }
        if let Some(s) = xkb.get("options").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                config.xkb_config.options = Some(s.to_string());
            }
        }
        if let Some(n) = xkb.get("repeat_rate").and_then(|v| v.as_integer()) {
            config.xkb_config.repeat_rate = n as u32;
        }
        if let Some(n) = xkb.get("repeat_delay").and_then(|v| v.as_integer()) {
            config.xkb_config.repeat_delay = n as u32;
        }
    }

    // Apply workspace overrides.
    if let Some(ws) = table.get("workspaces").and_then(|v| v.as_table()) {
        if let Some(s) = ws.get("workspace_layout").and_then(|v| v.as_str()) {
            config.workspaces.workspace_layout = match s {
                "Vertical" | "vertical" => cosmic_comp_config::workspace::WorkspaceLayout::Vertical,
                _ => cosmic_comp_config::workspace::WorkspaceLayout::Horizontal,
            };
        }
    }

    // Apply mouse overrides (maps to cosmic_conf.input_default).
    parse_mouse_config(&table, &mut config.input_default);
    // Apply touchpad overrides (maps to cosmic_conf.input_touchpad).
    parse_touchpad_config(&table, &mut config.input_touchpad);

    tracing::info!(
        "loaded compositor config from {} (xkb layout={:?})",
        path.display(),
        config.xkb_config.layout,
    );

    // An empty TOML `[xkb_config].layout` is the *expected* default for
    // most users — the `Config::xkb_config()` method will fill it later
    // from the fallback chain ($XKB_DEFAULT_LAYOUT → `localectl` →
    // `/etc/vconsole.conf`). A WARN at this point caused noisy spam
    // every config reload and confused users into thinking their
    // keyboard was broken. Keep it at DEBUG so the resolution path can
    // still be inspected when it's actually needed.
    if config.xkb_config.layout.is_empty() {
        tracing::debug!(
            "no explicit [xkb_config].layout in {} — will use fallback \
             chain (XKB_DEFAULT_LAYOUT → localectl → /etc/vconsole.conf)",
            path.display()
        );
    }

    let layout = parse_layout_config(&table);
    let keybindings = parse_keybindings_config(&table);
    let system_actions = parse_system_actions(&table);

    TomlConfig {
        cosmic: config,
        layout,
        keybindings,
        system_actions,
    }
}

/// Read the `[system_actions]` table from compositor.toml. Keys are
/// `shortcuts::action::System` enum variants serialised as strings
/// (e.g. `"VolumeRaise"`, `"BrightnessDown"`); values are the spawn
/// commands or `shell:` events that fire when the action triggers.
///
/// Unknown keys are warned-and-skipped so a typo can't poison
/// startup. The defaults table from `default_system_actions()` is
/// always present; the user overrides win on conflict.
fn parse_system_actions(
    table: &toml::Table,
) -> BTreeMap<shortcuts::action::System, String> {
    let Some(user_table) = table.get("system_actions").and_then(|v| v.as_table()) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<shortcuts::action::System, String> = BTreeMap::new();
    for (key, value) in user_table {
        let Some(command) = value.as_str() else {
            warn!(
                key = %key,
                "[system_actions] value must be a string, skipping"
            );
            continue;
        };
        // Round-trip the key string through serde to map it onto the
        // System enum. Unknown variants come back as a deserialise
        // error, which we log as a warning and skip.
        match toml::Value::String(key.clone())
            .try_into::<shortcuts::action::System>()
        {
            Ok(action) => {
                out.insert(action, command.to_string());
            }
            Err(err) => {
                warn!(
                    key = %key,
                    ?err,
                    "[system_actions] unknown action name, skipping"
                );
            }
        }
    }
    out
}

/// Overlay any persisted runtime-state overrides onto a freshly
/// parsed `CosmicCompConfig`. Used by both initial load and the
/// `toml_config_changed` hot-reload path so the precedence rule
/// stays uniform — fixes the medium finding from compositor #29
/// review (TOML hot-reload was reverting in-session toggles).
fn apply_runtime_state_overrides(
    cosmic_conf: &mut CosmicCompConfig,
    runtime_state: &LunarisRuntimeState,
) {
    if let Some(autotile) = runtime_state.autotile {
        cosmic_conf.autotile = autotile;
    }
    if let Some(pinned) = &runtime_state.pinned_workspaces {
        cosmic_conf.pinned_workspaces = pinned.clone();
    }
}

/// Convert our parsed Lunaris keybindings into the cosmic-shape
/// `Shortcuts` table. Several dispatch paths
/// (`input/mod.rs::handle_keyboard_event`, the resize-mode arrow
/// handler, the tiling-swap grab, the resize indicator) iterate
/// `(Binding, Action)` in cosmic terms; populating this from our
/// TOML keeps those code paths working without a wide rewrite.
///
/// Lunaris-private actions (`shell:`, `module:`, scratchpad,
/// monocle, etc.) are filtered out here — the cosmic table only
/// holds cosmic-`Action` variants. Private actions still dispatch
/// through the `toml_keybindings` loop in input handling.
fn build_cosmic_shortcuts(toml_keybindings: &[KeyBinding]) -> Shortcuts {
    use cosmic_settings_config::shortcuts::{Binding, Modifiers as CosmicModifiers};
    use std::collections::HashMap;

    let mut map: HashMap<Binding, shortcuts::Action> = HashMap::new();
    for kb in toml_keybindings {
        let Some(action) = key_bindings::action_from_str(&kb.action) else {
            continue;
        };
        let cosmic_action = match action {
            key_bindings::Action::Shortcut(a) => a,
            // Lunaris-private actions are dispatched directly from
            // the toml_keybindings loop in input/mod.rs.
            key_bindings::Action::Private(_) => continue,
        };
        let mods = CosmicModifiers {
            ctrl: kb.modifiers.ctrl,
            alt: kb.modifiers.alt,
            shift: kb.modifiers.shift,
            logo: kb.modifiers.super_key,
        };
        let key = key_bindings::keysym_from_str(&kb.key);
        let binding = Binding::new(mods, key);
        map.insert(binding, cosmic_action);
    }
    Shortcuts(map)
}

/// Map a cosmic-side `shortcuts::Action` value to the action
/// string used inside Lunaris' `compositor.toml [keybindings]`
/// table. Inverse of the prefix-aware mapping in
/// `key_bindings::action_from_str`. Used by `shortcut_for_action`
/// to find the user-visible accelerator label for a context-menu
/// item.
///
/// Variants without a TOML representation (e.g. `ToggleStacking`,
/// which is invoked from the menu but not user-bindable yet) fall
/// through with `None` — the menu code handles `None` by hiding
/// the shortcut label.
fn cosmic_action_to_action_string(
    action: &shortcuts::Action,
) -> Option<&'static str> {
    use cosmic_settings_config::shortcuts::Action as A;
    Some(match action {
        A::Close => "close_window",
        A::Minimize => "minimize",
        A::Maximize => "maximize",
        A::Fullscreen => "fullscreen",
        A::ToggleWindowFloating => "toggle_window_floating",
        A::ToggleTiling => "toggle_tiling",
        A::SwapWindow => "swap_window",
        A::NextWorkspace => "workspace_next",
        A::PreviousWorkspace => "workspace_prev",
        _ => return None,
    })
}

/// Render a keybinding (modifiers + key) as the
/// "Super+Shift+Q"-style string the context-menu UI expects.
fn format_keybinding(mods: &KeyBindingModifiers, key: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods.super_key {
        parts.push("Super");
    }
    if mods.ctrl {
        parts.push("Ctrl");
    }
    if mods.alt {
        parts.push("Alt");
    }
    if mods.shift {
        parts.push("Shift");
    }
    parts.push(key);
    parts.join("+")
}

/// Built-in defaults for the system-action map. These are what
/// users get when they ship a compositor.toml without a
/// `[system_actions]` section, and they replace the prior
/// `cosmic-settings-daemon` IPC inheritance once CC3 lands.
///
/// The commands intentionally use generic CLI tools (`wpctl`,
/// `playerctl`, `loginctl`, `xdg-open`) so the defaults work on a
/// fresh Lunaris install without extra wiring. Distros or users
/// can override individual entries via `[system_actions]`.
pub fn default_system_actions() -> BTreeMap<shortcuts::action::System, String> {
    use shortcuts::action::System;
    let mut m = BTreeMap::new();
    m.insert(
        System::VolumeRaise,
        "spawn:wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+".into(),
    );
    m.insert(
        System::VolumeLower,
        "spawn:wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-".into(),
    );
    m.insert(
        System::Mute,
        "spawn:wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle".into(),
    );
    m.insert(
        System::MuteMic,
        "spawn:wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle".into(),
    );
    // Brightness goes through the shell-overlay protocol so the
    // already-built coalesced step worker (D3.6) handles it; falling
    // back to brightnessctl would bypass the gamma-corrected slider
    // math and the persistence path.
    m.insert(System::BrightnessUp, "shell:brightness_up".into());
    m.insert(System::BrightnessDown, "shell:brightness_down".into());
    m.insert(System::PlayPause, "spawn:playerctl play-pause".into());
    m.insert(System::PlayNext, "spawn:playerctl next".into());
    m.insert(System::PlayPrev, "spawn:playerctl previous".into());
    m.insert(
        System::LockScreen,
        "spawn:loginctl lock-session".into(),
    );
    m.insert(System::Suspend, "spawn:systemctl suspend".into());
    m.insert(
        System::PowerOff,
        "spawn:systemctl poweroff".into(),
    );
    m.insert(System::LogOut, "spawn:loginctl terminate-session $XDG_SESSION_ID".into());
    m.insert(System::HomeFolder, "spawn:xdg-open ~".into());
    m.insert(System::WebBrowser, "spawn:xdg-open https:".into());
    m.insert(System::Launcher, "shell:waypointer_open".into());
    m.insert(System::AppLibrary, "shell:waypointer_open".into());
    m.insert(
        System::WindowSwitcher,
        "shell:workspace_map_open".into(),
    );
    m.insert(System::Screenshot, "spawn:grim".into());
    m
}

/// Parse layout configuration from the TOML table.
fn parse_layout_config(table: &toml::Table) -> LayoutConfig {
    let mut layout = LayoutConfig::default();

    let Some(section) = table.get("layout").and_then(|v| v.as_table()) else {
        return layout;
    };

    if let Some(n) = section.get("inner_gap").and_then(|v| v.as_integer()) {
        layout.inner_gap = n as i32;
    }
    if let Some(n) = section.get("outer_gap").and_then(|v| v.as_integer()) {
        layout.outer_gap = n as i32;
    }
    if let Some(b) = section.get("smart_gaps").and_then(|v| v.as_bool()) {
        layout.smart_gaps = b;
    }
    if let Some(b) = section.get("tiled_headers").and_then(|v| v.as_bool()) {
        layout.tiled_headers = b;
    }

    // Parse [[layout.window_rules]] array.
    if let Some(rules) = section.get("window_rules").and_then(|v| v.as_array()) {
        for rule_val in rules {
            let Some(rule_table) = rule_val.as_table() else { continue };
            let action = match rule_table.get("action").and_then(|v| v.as_str()) {
                Some("float") => WindowAction::Float,
                Some("tile") => WindowAction::Tile,
                _ => continue,
            };
            let matcher = if let Some(m) = rule_table.get("match").and_then(|v| v.as_table()) {
                WindowMatch {
                    app_id: m
                        .get("app_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| regex::Regex::new(s).ok()),
                    title: m
                        .get("title")
                        .and_then(|v| v.as_str())
                        .and_then(|s| regex::Regex::new(s).ok()),
                    window_type: m
                        .get("window_type")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                }
            } else {
                continue;
            };
            layout.window_rules.push(WindowRule { matcher, action });
        }
    }

    tracing::info!(
        "layout config: inner_gap={} outer_gap={} smart_gaps={} \
         tiled_headers={} rules={}",
        layout.inner_gap,
        layout.outer_gap,
        layout.smart_gaps,
        layout.tiled_headers,
        layout.window_rules.len(),
    );

    layout
}

/// Apply `[mouse]` TOML overrides onto the compositor's default pointer config.
///
/// Missing fields leave the current value untouched. An empty or absent
/// `[mouse]` section is a no-op. Values are clamped to libinput's accepted
/// range (acceleration speed -1.0..=1.0).
fn parse_mouse_config(table: &toml::Table, input: &mut InputConfig) {
    let Some(section) = table.get("mouse").and_then(|v| v.as_table()) else {
        return;
    };

    if let Some(f) = section.get("acceleration").and_then(|v| v.as_float()) {
        let speed = f.clamp(-1.0, 1.0);
        let mut accel = input.acceleration.clone().unwrap_or(AccelConfig {
            profile: None,
            speed,
        });
        accel.speed = speed;
        input.acceleration = Some(accel);
    }
    if let Some(b) = section.get("natural_scroll").and_then(|v| v.as_bool()) {
        let mut scroll = input.scroll_config.clone().unwrap_or(ScrollConfig {
            method: None,
            natural_scroll: None,
            scroll_button: None,
            scroll_factor: None,
        });
        scroll.natural_scroll = Some(b);
        input.scroll_config = Some(scroll);
    }
    if let Some(b) = section.get("left_handed").and_then(|v| v.as_bool()) {
        input.left_handed = Some(b);
    }
    // `scroll_speed` maps to libinput's `scroll_factor`: a linear
    // multiplier on the per-axis scroll delta. The sensible range is
    // roughly 0.1..3.0 — clamp there so an over-enthusiastic TOML
    // edit doesn't send the page flying on every tick.
    if let Some(f) = section.get("scroll_speed").and_then(|v| v.as_float()) {
        let factor = f.clamp(0.1, 3.0);
        let mut scroll = input.scroll_config.clone().unwrap_or(ScrollConfig {
            method: None,
            natural_scroll: None,
            scroll_button: None,
            scroll_factor: None,
        });
        scroll.scroll_factor = Some(factor);
        input.scroll_config = Some(scroll);
    }

    tracing::info!(
        "mouse config: accel={:?} natural_scroll={:?} left_handed={:?} scroll_factor={:?}",
        input.acceleration.as_ref().map(|a| a.speed),
        input.scroll_config.as_ref().and_then(|s| s.natural_scroll),
        input.left_handed,
        input.scroll_config.as_ref().and_then(|s| s.scroll_factor),
    );
}

/// Apply `[touchpad]` TOML overrides onto the compositor's default touchpad config.
fn parse_touchpad_config(table: &toml::Table, input: &mut InputConfig) {
    let Some(section) = table.get("touchpad").and_then(|v| v.as_table()) else {
        return;
    };

    if let Some(b) = section.get("tap_to_click").and_then(|v| v.as_bool()) {
        let mut tap = input.tap_config.clone().unwrap_or(TapConfig {
            enabled: b,
            button_map: None,
            drag: true,
            drag_lock: false,
        });
        tap.enabled = b;
        input.tap_config = Some(tap);
    }
    if let Some(b) = section.get("natural_scroll").and_then(|v| v.as_bool()) {
        let mut scroll = input.scroll_config.clone().unwrap_or(ScrollConfig {
            method: None,
            natural_scroll: None,
            scroll_button: None,
            scroll_factor: None,
        });
        scroll.natural_scroll = Some(b);
        input.scroll_config = Some(scroll);
    }
    if let Some(b) = section.get("two_finger_scroll").and_then(|v| v.as_bool()) {
        let mut scroll = input.scroll_config.clone().unwrap_or(ScrollConfig {
            method: None,
            natural_scroll: None,
            scroll_button: None,
            scroll_factor: None,
        });
        scroll.method = if b {
            Some(ScrollMethod::TwoFinger)
        } else {
            Some(ScrollMethod::NoScroll)
        };
        input.scroll_config = Some(scroll);
    }
    if let Some(b) = section.get("disable_while_typing").and_then(|v| v.as_bool()) {
        input.disable_while_typing = Some(b);
    }
    if let Some(f) = section.get("acceleration").and_then(|v| v.as_float()) {
        let speed = f.clamp(-1.0, 1.0);
        let mut accel = input.acceleration.clone().unwrap_or(AccelConfig {
            profile: None,
            speed,
        });
        accel.speed = speed;
        input.acceleration = Some(accel);
    }
    // Click method: `"clickfinger"` (default) uses finger count to
    // synthesise right/middle click; `"areas"` splits the bottom edge
    // into three hit zones like a physical trackpad.
    if let Some(s) = section.get("click_method").and_then(|v| v.as_str()) {
        input.click_method = match s {
            "areas" | "buttonareas" | "button_areas" => Some(ClickMethod::ButtonAreas),
            "clickfinger" => Some(ClickMethod::Clickfinger),
            other => {
                tracing::warn!(
                    "touchpad.click_method: unknown value {:?} (use 'clickfinger' or 'areas')",
                    other
                );
                None
            }
        };
    }
    // `tap_drag` is part of the `TapConfig` struct that tap_to_click
    // also populates, so we may have already created the config above.
    if let Some(b) = section.get("tap_drag").and_then(|v| v.as_bool()) {
        let mut tap = input.tap_config.clone().unwrap_or(TapConfig {
            enabled: true,
            button_map: None,
            drag: b,
            drag_lock: false,
        });
        tap.drag = b;
        input.tap_config = Some(tap);
    }

    tracing::info!(
        "touchpad config: tap={:?} natural_scroll={:?} dwt={:?} accel={:?} click_method={:?} tap_drag={:?}",
        input.tap_config.as_ref().map(|t| t.enabled),
        input.scroll_config.as_ref().and_then(|s| s.natural_scroll),
        input.disable_while_typing,
        input.acceleration.as_ref().map(|a| a.speed),
        input.click_method,
        input.tap_config.as_ref().map(|t| t.drag),
    );
}

/// Default keybindings used when no `[keybindings]` section is present.
pub fn default_keybindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    let defaults = [
        // Window / focus / move
        ("Super+T", "toggle_tiling"),
        ("Super+Shift+Space", "toggle_window_floating"),
        ("Super+H", "focus_left"),
        ("Super+J", "focus_down"),
        ("Super+K", "focus_up"),
        ("Super+L", "focus_right"),
        ("Super+Shift+H", "move_left"),
        ("Super+Shift+J", "move_down"),
        ("Super+Shift+K", "move_up"),
        ("Super+Shift+L", "move_right"),
        ("Super+F", "fullscreen"),
        ("Super+Q", "close_window"),
        ("Super+M", "toggle_monocle"),
        ("Super+Minus", "scratchpad_toggle"),
        ("Super+Shift+Minus", "scratchpad_move"),
        // Workspace switch (Super+1..9)
        ("Super+1", "workspace_switch:1"),
        ("Super+2", "workspace_switch:2"),
        ("Super+3", "workspace_switch:3"),
        ("Super+4", "workspace_switch:4"),
        ("Super+5", "workspace_switch:5"),
        ("Super+6", "workspace_switch:6"),
        ("Super+7", "workspace_switch:7"),
        ("Super+8", "workspace_switch:8"),
        ("Super+9", "workspace_switch:9"),
        // Workspace move (Super+Shift+1..9)
        ("Super+Shift+1", "workspace_move:1"),
        ("Super+Shift+2", "workspace_move:2"),
        ("Super+Shift+3", "workspace_move:3"),
        ("Super+Shift+4", "workspace_move:4"),
        ("Super+Shift+5", "workspace_move:5"),
        ("Super+Shift+6", "workspace_move:6"),
        ("Super+Shift+7", "workspace_move:7"),
        ("Super+Shift+8", "workspace_move:8"),
        ("Super+Shift+9", "workspace_move:9"),
        // App launchers and shell
        ("Super+Return", "spawn:foot"),
        ("Super+Space", "shell:waypointer_open"),
        ("Super+Tab", "shell:workspace_map_open"),
        // Hardware brightness keys (laptop Fn-row). The shell-side
        // handler reads the current backlight via logind and
        // steps it ±5 % per press. See `D3.6` in
        // `docs/architecture/display-system.md`.
        ("XF86MonBrightnessUp", "shell:brightness_up"),
        ("XF86MonBrightnessDown", "shell:brightness_down"),
    ];
    for (key_str, action) in defaults {
        if let Some((modifiers, key)) = parse_keybinding(key_str) {
            bindings.push(KeyBinding {
                modifiers,
                key,
                action: action.to_string(),
            });
        }
    }
    bindings
}

/// Parse keybindings from the TOML `[keybindings]` table.
///
/// If no `[keybindings]` section exists, default keybindings are used.
/// If the section exists (even if empty), only the configured bindings
/// are active (explicit override).
fn parse_keybindings_config(table: &toml::Table) -> Vec<KeyBinding> {
    let Some(section) = table.get("keybindings").and_then(|v| v.as_table()) else {
        return default_keybindings();
    };

    let mut bindings = Vec::new();
    for (key_str, action_val) in section {
        let Some(action) = action_val.as_str() else { continue };
        if let Some((modifiers, key)) = parse_keybinding(key_str) {
            bindings.push(KeyBinding {
                modifiers,
                key,
                action: action.to_string(),
            });
        }
    }

    if !bindings.is_empty() {
        tracing::info!("loaded {} keybindings from TOML", bindings.len());
        for kb in &bindings {
            tracing::info!(
                "  keybinding: {:?}+{:?} -> {:?}",
                kb.modifiers, kb.key, kb.action,
            );
        }
    } else {
        tracing::info!("no keybindings found in TOML [keybindings] section");
    }

    bindings
}

impl Config {
    pub fn load(loop_handle: &LoopHandle<'_, State>) -> Config {
        let xdg = xdg::BaseDirectories::new();

        // Load compositor config from TOML.
        let toml_path = std::env::var("LUNARIS_COMPOSITOR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join(DEFAULT_TOML_PATH)
            });

        let toml_config = load_toml_config(&toml_path);
        let cosmic_comp_config = toml_config.cosmic;
        let layout_config = toml_config.layout;
        let toml_keybindings = toml_config.keybindings;

        // Watch the TOML config file for changes via notify.
        if let Some(parent) = toml_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        {
            let watch_path = toml_path.clone();
            let (notify_tx, notify_rx) = calloop::channel::channel::<()>();
            let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
                if let Ok(event) = res {
                    // Editors do atomic writes (rename), so watch for Create and Modify.
                    use notify::EventKind;
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        let _ = notify_tx.send(());
                    }
                }
            })
            .expect("failed to create config file watcher");

            // Watch the parent directory (editors do atomic renames).
            if let Some(parent) = watch_path.parent() {
                use notify::Watcher;
                watcher
                    .watch(parent, notify::RecursiveMode::NonRecursive)
                    .expect("failed to watch config directory");
            }

            // Also watch the keybinding fragment directory so module
            // installs / uninstalls propagate without restarting the
            // compositor. Best-effort: if the directory does not exist
            // yet (no module has been installed), skip — `installd`
            // creates it on the first write and the next reload picks
            // it up.
            {
                let fragment_dir = keybinding_fragment_dir(&watch_path);
                if fragment_dir.exists() {
                    use notify::Watcher;
                    if let Err(err) = watcher.watch(
                        &fragment_dir,
                        notify::RecursiveMode::NonRecursive,
                    ) {
                        tracing::warn!(
                            "could not watch keybinding fragment dir {}: {err}",
                            fragment_dir.display()
                        );
                    }
                }
            }

            let toml_path_for_handler = watch_path;
            loop_handle
                .insert_source(notify_rx, move |_, _, state| {
                    toml_config_changed(&toml_path_for_handler, state);
                })
                .expect("failed to add config watcher to event loop");

            // Leak the watcher so it stays alive for the process lifetime.
            // Same pattern as theme.rs (std::mem::forget on watchers).
            std::mem::forget(watcher);
        }

        // Build the system-actions map from defaults plus the
        // user's `[system_actions]` overrides. The
        // cosmic-settings-daemon source was removed in
        // compositor #29 / CC3 — Lunaris no longer inherits
        // anything from `com.system76.CosmicSettings.*`.
        let mut system_actions = default_system_actions();
        for (action, command) in toml_config.system_actions.clone() {
            system_actions.insert(action, command);
        }

        // Build the cosmic-shape Shortcuts table from our
        // toml_keybindings list. The dispatch loops in
        // input/mod.rs and the tiling/resize indicators iterate
        // this in cosmic-Action terms; populating it locally
        // keeps those paths functional while the cosmic-settings
        // source disappears.
        let shortcuts = build_cosmic_shortcuts(&toml_keybindings);

        // tiling_exceptions formerly populated from
        // `com.system76.CosmicSettings.WindowRules`. That source is
        // gone — Lunaris-side window rules live in
        // `compositor.toml [layout].window_rules` and flow through
        // the `Config.layout.window_rules` field instead. We keep
        // this Vec empty so `shell::TilingExceptions::new` and
        // friends still compile.
        let tiling_exceptions: Vec<ApplicationException> = Vec::new();

        let _ = loop_handle.insert_idle(|state| {
            let filter_conf = state.common.config.dynamic_conf.screen_filter();
            state
                .common
                .a11y_state
                .set_screen_inverted(filter_conf.inverted);
            state
                .common
                .a11y_state
                .set_screen_filter(filter_conf.color_filter);
        });

        // Load runtime state, then merge any explicit overrides
        // into `cosmic_comp_config`. Precedence rule (compositor
        // #29 review HIGH 2): runtime state only wins when the
        // user has actually persisted the field (`Some(_)`); a
        // missing or default state file leaves the TOML value
        // intact.
        let dynamic_conf = Self::load_dynamic(&xdg);
        let mut cosmic_comp_config = cosmic_comp_config;
        apply_runtime_state_overrides(
            &mut cosmic_comp_config,
            dynamic_conf.runtime_state(),
        );

        Config {
            dynamic_conf,
            cosmic_conf: cosmic_comp_config,
            toml_path,
            shortcuts,
            tiling_exceptions,
            layout: layout_config,
            toml_keybindings,
            system_actions,
            nested: false,
        }
    }

    fn load_dynamic(xdg: &xdg::BaseDirectories) -> DynamicConfig {
        // Output config moved from `~/.local/state/cosmic-comp/outputs.ron`
        // to `~/.config/lunaris/compositor.d/displays.toml`. See
        // `docs/architecture/display-system.md` §A1. On first boot
        // after the change, the legacy RON is converted to TOML and
        // unlinked. The legacy path keeps working for `numlock` and
        // `a11y_screen_filter` because those are dev-time state with
        // no settings UI yet.
        let displays_path = displays_toml_path();
        if let Some(toml_path) = displays_path.as_ref() {
            if let Ok(legacy_ron) = xdg.place_state_file("cosmic-comp/outputs.ron") {
                cosmic_comp_config::output::displays_toml::migrate_from_ron(
                    &legacy_ron,
                    toml_path,
                );
            }
        }
        let outputs = displays_path
            .as_deref()
            .map(cosmic_comp_config::output::displays_toml::load)
            .unwrap_or_else(|| OutputsConfig {
                config: Default::default(),
            });

        let numlock_path = xdg.place_state_file("cosmic-comp/numlock.ron").ok();
        let numlock = Self::load_numlock(&numlock_path);

        let filter_path = xdg
            .place_state_file("cosmic-comp/a11y_screen_filter.ron")
            .ok();
        let filter = Self::load_filter_state(&filter_path);

        let runtime_state_path = xdg
            .place_state_file("lunaris/compositor/state.toml")
            .ok();
        let runtime_state = Self::load_runtime_state(&runtime_state_path);

        DynamicConfig {
            outputs: (displays_path, outputs),
            numlock: (numlock_path, numlock),
            accessibility_filter: (filter_path, filter),
            runtime_state: (runtime_state_path, runtime_state),
        }
    }

    /// Read `~/.local/state/lunaris/compositor/state.toml`. Missing
    /// or unparseable file returns defaults — this state can always
    /// be regenerated from the next user toggle, so a corrupt file
    /// is logged-and-replaced rather than fatal.
    fn load_runtime_state(path: &Option<PathBuf>) -> LunarisRuntimeState {
        let Some(path) = path.as_deref() else {
            return LunarisRuntimeState::default();
        };
        if !path.exists() {
            return LunarisRuntimeState::default();
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<LunarisRuntimeState>(&content) {
                Ok(state) => state,
                Err(err) => {
                    warn!(
                        ?err,
                        "Failed to parse lunaris compositor runtime state, resetting.."
                    );
                    let _ = std::fs::remove_file(path);
                    LunarisRuntimeState::default()
                }
            },
            Err(err) => {
                warn!(?err, "Failed to read lunaris compositor runtime state");
                LunarisRuntimeState::default()
            }
        }
    }

    fn load_numlock(path: &Option<PathBuf>) -> NumlockStateConfig {
        path.as_deref()
            .filter(|path| path.exists())
            .and_then(|path| {
                ron::de::from_reader::<_, NumlockStateConfig>(
                    OpenOptions::new().read(true).open(path).unwrap(),
                )
                .map_err(|err| {
                    warn!(?err, "Failed to read numlock.ron, resetting..");
                    if let Err(err) = std::fs::remove_file(path) {
                        error!(?err, "Failed to remove numlock.ron.");
                    }
                })
                .ok()
            })
            .unwrap_or_default()
    }

    fn load_filter_state(path: &Option<PathBuf>) -> ScreenFilter {
        if let Some(path) = path.as_ref()
            && path.exists()
        {
            match ron::de::from_reader::<_, ScreenFilter>(
                OpenOptions::new().read(true).open(path).unwrap(),
            ) {
                Ok(config) => return config,
                Err(err) => {
                    warn!(?err, "Failed to read screen_filter state, resetting..");
                    if let Err(err) = std::fs::remove_file(path) {
                        error!(?err, "Failed to remove screen_filter state.");
                    }
                }
            };
        }

        ScreenFilter {
            inverted: false,
            color_filter: None,
            night_light_tint: None,
        }
    }

    pub fn shortcut_for_action(&self, action: &shortcuts::Action) -> Option<String> {
        let action_str = cosmic_action_to_action_string(action)?;
        self.toml_keybindings
            .iter()
            .find(|kb| kb.action == action_str)
            .map(|kb| format_keybinding(&kb.modifiers, &kb.key))
    }

    pub fn read_outputs(
        &mut self,
        output_state: &mut OutputConfigurationState<State>,
        backend: &mut BackendData,
        shell: &Arc<parking_lot::RwLock<Shell>>,
        loop_handle: &LoopHandle<'static, State>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        xdg_activation_state: &XdgActivationState,
        startup_done: Arc<AtomicBool>,
        clock: &Clock<Monotonic>,
    ) -> anyhow::Result<()> {
        let outputs = output_state.outputs().collect::<Vec<_>>();
        let mut infos = outputs
            .iter()
            .cloned()
            .map(Into::<crate::config::CompOutputInfo>::into)
            .map(|i| i.0)
            .collect::<Vec<_>>();
        infos.sort();

        if let Some(configs) = self
            .dynamic_conf
            .outputs()
            .config
            .get(&infos)
            .filter(|configs| {
                if configs
                    .iter()
                    .all(|config| config.enabled == OutputState::Disabled)
                {
                    if !configs.is_empty() {
                        error!(
                            "Broken config, all outputs disabled. Resetting... {:?}",
                            configs
                        );
                    }
                    false
                } else {
                    true
                }
            })
            .cloned()
        {
            let known_good_configs = outputs
                .iter()
                .map(|output| {
                    output
                        .user_data()
                        .get::<RefCell<OutputConfig>>()
                        .unwrap()
                        .borrow()
                        .clone()
                })
                .collect::<Vec<_>>();

            let mut found_outputs = Vec::new();
            for (name, output_config) in infos.iter().map(|o| &o.connector).zip(configs.into_iter())
            {
                let output = outputs.iter().find(|o| &o.name() == name).unwrap().clone();
                let enabled = output_config.enabled.clone();
                *output
                    .user_data()
                    .get::<RefCell<OutputConfig>>()
                    .unwrap()
                    .borrow_mut() = output_config;
                found_outputs.push((output.clone(), enabled));
            }

            let mut backend = backend.lock();
            if let Err(err) = backend.apply_config_for_outputs(
                false,
                loop_handle,
                self.dynamic_conf.screen_filter(),
                shell.clone(),
                workspace_state,
                xdg_activation_state,
                startup_done.clone(),
                clock,
            ) {
                warn!(?err, "Failed to set new config.");
                found_outputs.clear();
                for (output, output_config) in outputs
                    .clone()
                    .into_iter()
                    .zip(known_good_configs.into_iter())
                {
                    let enabled = output_config.enabled.clone();
                    *output
                        .user_data()
                        .get::<RefCell<OutputConfig>>()
                        .unwrap()
                        .borrow_mut() = output_config;
                    found_outputs.push((output.clone(), enabled));
                }

                backend
                    .apply_config_for_outputs(
                        false,
                        loop_handle,
                        self.dynamic_conf.screen_filter(),
                        shell.clone(),
                        workspace_state,
                        xdg_activation_state,
                        startup_done,
                        clock,
                    )
                    .context("Failed to reset config")?;

                for (output, enabled) in found_outputs {
                    if enabled == OutputState::Enabled {
                        output_state.enable_head(&output);
                    } else {
                        output_state.disable_head(&output);
                    }
                }
            } else {
                for (output, enabled) in found_outputs {
                    if enabled == OutputState::Enabled {
                        output_state.enable_head(&output);
                    } else {
                        output_state.disable_head(&output);
                    }
                }
            }

            output_state.update();
            self.write_outputs(output_state.outputs());
        } else {
            if outputs
                .iter()
                .all(|o| o.config().enabled == OutputState::Disabled)
            {
                for output in &outputs {
                    output.config_mut().enabled = OutputState::Enabled;
                }
            }

            // we don't have a config, so lets generate somewhat sane positions
            let mut w = 0;
            if !outputs.iter().any(|o| o.config().xwayland_primary) {
                // if we don't have a primary output for xwayland from a previous config, pick one
                if let Some(primary) = outputs.iter().find(|o| o.mirroring().is_none()) {
                    primary.config_mut().xwayland_primary = true;
                }
            }
            for output in outputs.iter().filter(|o| o.mirroring().is_none()) {
                {
                    let mut config = output.config_mut();
                    config.position = (w, 0);
                }
                w += output.geometry().size.w as u32;
            }

            let mut backend = backend.lock();
            backend
                .apply_config_for_outputs(
                    false,
                    loop_handle,
                    self.dynamic_conf.screen_filter(),
                    shell.clone(),
                    workspace_state,
                    xdg_activation_state,
                    startup_done.clone(),
                    clock,
                )
                .context("Failed to set new config")?;

            for output in outputs {
                if output
                    .user_data()
                    .get::<RefCell<OutputConfig>>()
                    .unwrap()
                    .borrow()
                    .enabled
                    == OutputState::Enabled
                {
                    output_state.enable_head(&output);
                } else {
                    output_state.disable_head(&output);
                }
            }
            output_state.update();
            self.write_outputs(output_state.outputs());
        }

        Ok(())
    }

    pub fn write_outputs(
        &mut self,
        outputs: impl Iterator<Item = impl std::borrow::Borrow<Output>>,
    ) {
        let mut infos = outputs
            .map(|o| {
                let o = o.borrow();
                (
                    Into::<CompOutputInfo>::into(o.clone()).0,
                    o.user_data()
                        .get::<RefCell<OutputConfig>>()
                        .unwrap()
                        .borrow()
                        .clone(),
                )
            })
            .collect::<Vec<(OutputInfo, OutputConfig)>>();
        infos.sort_by(|(a, _), (b, _)| a.cmp(b));
        let (infos, configs) = infos.into_iter().unzip();
        self.dynamic_conf
            .outputs_mut()
            .config
            .insert(infos, configs);
    }

    pub fn xkb_config(&self) -> XkbConfig {
        let mut cfg = self.cosmic_conf.xkb_config.clone();
        // If the layout is empty (no cosmic-config or TOML config set it),
        // fall back to environment variables, then /etc/vconsole.conf.
        if cfg.layout.is_empty() {
            if let Ok(layout) = std::env::var("XKB_DEFAULT_LAYOUT") {
                cfg.layout = layout;
            }
        }
        if cfg.variant.is_empty() {
            if let Ok(variant) = std::env::var("XKB_DEFAULT_VARIANT") {
                cfg.variant = variant;
            }
        }
        if cfg.model.is_empty() {
            if let Ok(model) = std::env::var("XKB_DEFAULT_MODEL") {
                cfg.model = model;
            }
        }
        if cfg.rules.is_empty() {
            if let Ok(rules) = std::env::var("XKB_DEFAULT_RULES") {
                cfg.rules = rules;
            }
        }
        if cfg.options.is_none() {
            if let Ok(options) = std::env::var("XKB_DEFAULT_OPTIONS") {
                if !options.is_empty() {
                    cfg.options = Some(options);
                }
            }
        }
        // If still empty, try localectl (systemd) which reads the full X11 config.
        if cfg.layout.is_empty() {
            let system = read_system_xkb_layout();
            if !system.layout.is_empty() {
                cfg.layout = system.layout;
            }
            if cfg.variant.is_empty() && !system.variant.is_empty() {
                cfg.variant = system.variant;
            }
            if cfg.model.is_empty() && !system.model.is_empty() {
                cfg.model = system.model;
            }
            if cfg.options.is_none() && !system.options.is_empty() {
                cfg.options = Some(system.options);
            }
        }
        // Last resort: /etc/vconsole.conf KEYMAP field.
        if cfg.layout.is_empty() {
            if let Some(vconsole) = parse_vconsole_keymap() {
                cfg.layout = vconsole;
            }
        }
        // If even the last-resort chain didn't find anything, the user
        // really has no resolvable XKB layout — warn once per process
        // so keyboard issues have a log trail instead of failing
        // silently. Noisy repeats are suppressed via `XKB_WARNED`.
        if cfg.layout.is_empty() {
            use std::sync::atomic::{AtomicBool, Ordering};
            static XKB_WARNED: AtomicBool = AtomicBool::new(false);
            if !XKB_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "unable to resolve XKB layout: no TOML [xkb_config], \
                     no XKB_DEFAULT_LAYOUT env, no `localectl` entry, \
                     no /etc/vconsole.conf KEYMAP — keyboard input may \
                     fall back to the XKB built-in default"
                );
            }
        } else {
            tracing::debug!(
                "xkb layout resolved: layout={:?} variant={:?} options={:?}",
                cfg.layout, cfg.variant, cfg.options
            );
        }
        cfg
    }

    pub fn read_device(&self, device: &mut InputDevice) {
        let (device_config, default_config) = self.get_device_config(device);
        input_config::update_device(device, device_config.as_ref(), default_config);
    }

    pub fn scroll_factor(&self, device: &InputDevice) -> f64 {
        let (device_config, default_config) = self.get_device_config(device);
        input_config::get_config(device_config.as_ref(), default_config, |x| {
            x.scroll_config.as_ref()?.scroll_factor
        })
        .map_or(1.0, |x| x.0)
    }

    pub fn map_to_output(&self, device: &InputDevice) -> Option<String> {
        let (device_config, default_config) = self.get_device_config(device);
        Some(
            input_config::get_config(device_config.as_ref(), default_config, |x| {
                x.map_to_output.clone()
            })?
            .0,
        )
    }

    fn get_device_config(&self, device: &InputDevice) -> (Option<InputConfig>, &InputConfig) {
        let is_touchpad = device.config_tap_finger_count() > 0;

        let default_config = if is_touchpad {
            &self.cosmic_conf.input_touchpad
        } else {
            &self.cosmic_conf.input_default
        };

        let mut device_config = self.cosmic_conf.input_devices.get(device.name()).cloned();
        if is_touchpad && self.cosmic_conf.input_touchpad_override == TouchpadOverride::ForceDisable
        {
            device_config = Some({
                let mut config = device_config.unwrap_or_default();
                config.state = InputDeviceState::Disabled;
                config
            });
        }

        (device_config, default_config)
    }
}

/// Per-type serializer callback. The original `PersistenceGuard`
/// hardcoded RON for everything; with `outputs.ron` migrating to
/// TOML the format now varies per dynamic-state file. The callback
/// closes over the value and produces the on-disk text.
type Serializer<T> = fn(&T) -> Result<String, String>;

fn ron_serialize<T: Serialize>(value: &T) -> Result<String, String> {
    ron::ser::to_string_pretty(value, Default::default()).map_err(|e| e.to_string())
}

fn outputs_toml_serialize(value: &OutputsConfig) -> Result<String, String> {
    cosmic_comp_config::output::displays_toml::to_toml_string(value)
}

pub struct PersistenceGuard<'a, T: Serialize> {
    path: Option<PathBuf>,
    value: &'a mut T,
    serialize: Serializer<T>,
}

impl<T: Serialize> std::ops::Deref for PersistenceGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: Serialize> std::ops::DerefMut for PersistenceGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

impl<T: Serialize> Drop for PersistenceGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_ref() {
            let content = match (self.serialize)(self.value) {
                Ok(content) => content,
                Err(err) => {
                    warn!("Failed to serialize: {err}");
                    return;
                }
            };

            // Make sure the parent directory exists. The TOML
            // outputs path lives under `~/.config/lunaris/compositor.d/`
            // which has no other guaranteed creator.
            let Some(parent) = path.parent() else {
                warn!("PersistenceGuard target path has no parent: {}", path.display());
                return;
            };
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(?err, "Failed to create parent dir for {}", path.display());
                return;
            }

            // Atomic write: write to `<file>.tmp`, fsync, rename over
            // the target. Without this a process kill between
            // truncate and write_all leaves the file half-formed —
            // and for `outputs`, that file is the only authoritative
            // copy of the user's display layout.
            let tmp_path = path.with_extension(
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| format!("{e}.tmp"))
                    .unwrap_or_else(|| "tmp".into()),
            );
            let write_result = (|| -> std::io::Result<()> {
                let mut writer = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&tmp_path)?;
                writer.write_all(content.as_bytes())?;
                writer.flush()?;
                writer.sync_all()?;
                std::fs::rename(&tmp_path, path)?;
                Ok(())
            })();

            if let Err(err) = write_result {
                warn!(?err, "Failed to persist {} atomically.", path.display());
                let _ = std::fs::remove_file(&tmp_path);
                return;
            }

            // Best-effort directory fsync so the rename itself is
            // durable. Failure is non-fatal.
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
}

impl DynamicConfig {
    pub fn outputs(&self) -> &OutputsConfig {
        &self.outputs.1
    }

    pub fn outputs_mut(&mut self) -> PersistenceGuard<'_, OutputsConfig> {
        PersistenceGuard {
            path: self.outputs.0.clone(),
            value: &mut self.outputs.1,
            serialize: outputs_toml_serialize,
        }
    }

    pub fn numlock(&self) -> &NumlockStateConfig {
        &self.numlock.1
    }

    pub fn numlock_mut(&mut self) -> PersistenceGuard<'_, NumlockStateConfig> {
        PersistenceGuard {
            path: self.numlock.0.clone(),
            value: &mut self.numlock.1,
            serialize: ron_serialize,
        }
    }

    pub fn screen_filter(&self) -> &ScreenFilter {
        &self.accessibility_filter.1
    }

    pub fn screen_filter_mut(&mut self) -> PersistenceGuard<'_, ScreenFilter> {
        PersistenceGuard {
            path: self.accessibility_filter.0.clone(),
            value: &mut self.accessibility_filter.1,
            serialize: ron_serialize,
        }
    }

    pub fn runtime_state(&self) -> &LunarisRuntimeState {
        &self.runtime_state.1
    }

    /// Mutable handle to the runtime-state file. The returned
    /// guard writes back to `state.toml` atomically (tmp + rename)
    /// when dropped. Use for any field on `LunarisRuntimeState`
    /// that needs to persist across sessions.
    pub fn runtime_state_mut(&mut self) -> PersistenceGuard<'_, LunarisRuntimeState> {
        PersistenceGuard {
            path: self.runtime_state.0.clone(),
            value: &mut self.runtime_state.1,
            serialize: lunaris_runtime_serialize,
        }
    }
}

fn lunaris_runtime_serialize(value: &LunarisRuntimeState) -> Result<String, String> {
    toml::to_string_pretty(value).map_err(|e| e.to_string())
}

/// Reads the system XKB layout from `localectl status`.
///
/// Parses lines like:
///   X11 Layout: de
///   X11 Variant: nodeadkeys
///   X11 Model: pc105
///   X11 Options: compose:ralt
struct SystemXkb {
    layout: String,
    variant: String,
    model: String,
    options: String,
}

fn read_system_xkb_layout() -> SystemXkb {
    let mut result = SystemXkb {
        layout: String::new(),
        variant: String::new(),
        model: String::new(),
        options: String::new(),
    };

    let output = match std::process::Command::new("localectl")
        .arg("status")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return result,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("X11 Layout:") {
            result.layout = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("X11 Variant:") {
            result.variant = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("X11 Model:") {
            result.model = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("X11 Options:") {
            result.options = v.trim().to_string();
        }
    }

    if !result.layout.is_empty() {
        tracing::info!(
            "read_system_xkb_layout: layout={} variant={} model={} options={}",
            result.layout, result.variant, result.model, result.options,
        );
    }

    result
}

/// Reads the KEYMAP setting from /etc/vconsole.conf.
///
/// Returns the layout string (e.g. "de", "us") or None if not found.
fn parse_vconsole_keymap() -> Option<String> {
    let content = std::fs::read_to_string("/etc/vconsole.conf").ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("KEYMAP=") {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub fn xkb_config_to_wl(config: &XkbConfig) -> WlXkbConfig<'_> {
    WlXkbConfig {
        rules: &config.rules,
        model: &config.model,
        layout: &config.layout,
        variant: &config.variant,
        options: config.options.clone(),
    }
}

fn update_input(state: &mut State) {
    if let BackendData::Kms(kms_state) = &mut state.backend {
        for device in kms_state.input_devices.values_mut() {
            state.common.config.read_device(device);
        }
    }
}

pub fn change_modifier_state(
    keyboard: &smithay::input::keyboard::KeyboardHandle<State>,
    scan_code: u32,
    state: &mut State,
) {
    /// Offset used to convert Linux scancode to X11 keycode.
    const X11_KEYCODE_OFFSET: u32 = 8;

    let mut input = |key_state, scan_code| {
        let time = state.common.clock.now().as_millis();
        let _ = keyboard.input(
            state,
            smithay_input::Keycode::new(scan_code + X11_KEYCODE_OFFSET),
            key_state,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| smithay::input::keyboard::FilterResult::<()>::Forward,
        );
    };

    input(smithay_input::KeyState::Pressed, scan_code);
    input(smithay_input::KeyState::Released, scan_code);
}

/// Called when the TOML config file changes. Reloads the full config and
/// applies side effects for any fields that differ from the current state.
/// Called when the TOML config file changes. Reloads the full config and
/// applies side effects for any fields that differ from the current state.
fn toml_config_changed(toml_path: &std::path::Path, state: &mut State) {
    let toml = load_toml_config(toml_path);
    let mut new = toml.cosmic;
    // Re-apply runtime-state overrides on hot-reload so an
    // unrelated TOML edit doesn't revert the user's in-session
    // toggle (autotile, pinned_workspaces). Same precedence rule
    // as initial load — fixes compositor #29 review MEDIUM.
    apply_runtime_state_overrides(
        &mut new,
        state.common.config.dynamic_conf.runtime_state(),
    );

    // Capture old toggle state before we replace the layout so we can
    // detect a tiled_headers flip and trigger a reconfigure storm only
    // when the bool actually changed.
    let old_tiled_headers = state.common.config.layout.tiled_headers;

    // Update layout config and keybindings.
    state.common.config.layout = toml.layout;
    state.common.config.toml_keybindings = toml.keybindings;
    // Rebuild the cosmic-shape Shortcuts table off the new
    // toml_keybindings so the dispatch loops in input/mod.rs see
    // the live update.
    state.common.config.shortcuts =
        build_cosmic_shortcuts(&state.common.config.toml_keybindings);

    // Rebuild system-actions from defaults + user overrides.
    // The cosmic-settings-daemon source is gone (CC3); the user's
    // `[system_actions]` table is the only override layer.
    let mut system_actions = default_system_actions();
    for (action, command) in toml.system_actions.clone() {
        system_actions.insert(action, command);
    }
    state.common.config.system_actions = system_actions;

    // Refresh the dynamic binding resolver's static snapshot so
    // D-Bus-registered bindings continue to see the right static
    // precedence after a hot-reload. Fragments from
    // compositor.d/keybindings.d/ are reloaded here too so module
    // install / uninstall takes effect without restarting the
    // compositor.
    {
        use crate::input::binding_resolver::{BindingScope, StaticBinding};
        let mut statics: Vec<StaticBinding> = state
            .common
            .config
            .toml_keybindings
            .iter()
            .map(|kb| StaticBinding::from_toml(kb, BindingScope::User))
            .collect();
        let fragment_dir = keybinding_fragment_dir(toml_path);
        for entry in load_keybinding_fragments(&fragment_dir) {
            if let Some(b) = StaticBinding::from_accelerator(
                &entry.binding,
                entry.action,
                BindingScope::Module,
            ) {
                statics.push(b);
            }
        }
        state.common.binding_resolver.set_static_bindings(statics);
    }

    // Propagate gap settings and window rules to the shell.
    {
        let layout = &state.common.config.layout;
        let mut shell = state.common.shell.write();
        for workspace in shell.workspaces.spaces_mut() {
            workspace.tiling_layer.set_gaps(
                layout.inner_gap,
                layout.outer_gap,
                layout.smart_gaps,
            );
            // Re-layout immediately so gap changes are visible without
            // waiting for the next window event.
            workspace.tiling_layer.recalculate();
        }
        shell.window_rules = layout.window_rules.clone();
        // Apply tiled_headers toggle only when the value actually changed.
        // The setter walks all tiled non-stack windows and sends a configure
        // roundtrip so the client repaints at the new size, then bumps the
        // theme generation so cached header textures are invalidated.
        if layout.tiled_headers != old_tiled_headers {
            shell.set_tiled_headers_enabled(layout.tiled_headers);
        }
    }

    // Compare old vs new to determine which side effects to trigger.
    // We clone the old config so we can assign the new one early and avoid
    // borrow conflicts with &state.
    let old = state.common.config.cosmic_conf.clone();
    state.common.config.cosmic_conf = new.clone();

    // xkb_config
    if new.xkb_config != old.xkb_config {
        let value = &new.xkb_config;
        let seats = state
            .common
            .shell
            .read()
            .seats
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for seat in seats.into_iter() {
            if let Some(keyboard) = seat.get_keyboard() {
                let old_modifier_state = keyboard.modifier_state();
                keyboard.change_repeat_info(
                    (value.repeat_rate as i32).abs(),
                    (value.repeat_delay as i32).abs(),
                );
                tracing::info!(
                    "xkb_config update: layout={:?} variant={:?} model={:?} options={:?}",
                    value.layout, value.variant, value.model, value.options,
                );
                if let Err(err) = keyboard.set_xkb_config(state, xkb_config_to_wl(value)) {
                    error!(?err, "Failed to load provided xkb config");
                }
                if old_modifier_state.num_lock != keyboard.modifier_state().num_lock {
                    const NUMLOCK_SCANCODE: u32 = 69;
                    change_modifier_state(&keyboard, NUMLOCK_SCANCODE, state);
                }
                if old_modifier_state.caps_lock != keyboard.modifier_state().caps_lock {
                    const CAPSLOCK_SCANCODE: u32 = 58;
                    change_modifier_state(&keyboard, CAPSLOCK_SCANCODE, state);
                }
            }
        }
    }

    // keyboard_config
    if new.keyboard_config != old.keyboard_config {
        let shell = state.common.shell.read();
        let seat = shell.seats.last_active();
        state.common.config.dynamic_conf.numlock_mut().last_state =
            seat.get_keyboard().unwrap().modifier_state().num_lock;
    }

    // input
    if new.input_default != old.input_default
        || new.input_touchpad != old.input_touchpad
        || new.input_touchpad_override != old.input_touchpad_override
        || new.input_devices != old.input_devices
    {
        update_input(state);
    }

    // workspaces
    if new.workspaces != old.workspaces {
        state.common.update_config();
    }

    // autotile
    if new.autotile != old.autotile {
        let mut shell = state.common.shell.write();
        let shell_ref = &mut *shell;
        shell_ref.workspaces.update_autotile(
            new.autotile,
            &mut state.common.workspace_state.update(),
            shell_ref.seats.iter(),
        );
    }

    // autotile_behavior
    if new.autotile_behavior != old.autotile_behavior {
        let mut shell = state.common.shell.write();
        let shell_ref = &mut *shell;
        shell_ref.workspaces.update_autotile_behavior(
            new.autotile_behavior,
            &mut state.common.workspace_state.update(),
            shell_ref.seats.iter(),
        );
    }

    // active_hint
    if new.active_hint != old.active_hint {
        state.common.update_config();
    }

    // descale_xwayland
    if new.descale_xwayland != old.descale_xwayland {
        state.common.update_xwayland_settings();
    }

    // xwayland_eavesdropping
    if new.xwayland_eavesdropping != old.xwayland_eavesdropping {
        state
            .common
            .xwayland_reset_eavesdropping(SERIAL_COUNTER.next_serial());
    }

    // accessibility_zoom
    if new.accessibility_zoom != old.accessibility_zoom {
        state.common.update_config();
    }

    // appearance_settings
    if new.appearance_settings != old.appearance_settings {
        state.common.update_config();
        for output in state.common.shell.read().outputs() {
            state.backend.schedule_render(output);
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompOutputInfo(OutputInfo);

impl From<Output> for CompOutputInfo {
    fn from(o: Output) -> CompOutputInfo {
        let physical = o.physical_properties();
        CompOutputInfo(OutputInfo {
            connector: o.name(),
            make: physical.make,
            model: physical.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_sparse_toml() {
        let dir = std::env::temp_dir().join("lunaris-config-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-compositor.toml");
        std::fs::write(
            &path,
            r#"
[xkb_config]
layout = "de"

[workspaces]
workspace_layout = "Horizontal"
"#,
        )
        .unwrap();

        let tc = load_toml_config(&path);
        assert_eq!(tc.cosmic.xkb_config.layout, "de", "layout must be 'de'");
        assert_eq!(tc.cosmic.xkb_config.repeat_rate, 25);
        // No [keybindings] section -> defaults are loaded.
        assert!(
            !tc.keybindings.is_empty(),
            "default keybindings should be loaded when no [keybindings] section"
        );
        assert!(
            tc.keybindings.iter().any(|k| k.action == "focus_left"),
            "default keybindings should include focus_left"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_load_empty_toml() {
        let dir = std::env::temp_dir().join("lunaris-config-test-empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-empty.toml");
        std::fs::write(&path, "").unwrap();

        let tc = load_toml_config(&path);
        assert_eq!(tc.cosmic.xkb_config.layout, "");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_load_missing_toml() {
        let tc = load_toml_config(std::path::Path::new("/nonexistent/path.toml"));
        assert_eq!(tc.cosmic.xkb_config.layout, "");
    }

    #[test]
    fn test_load_layout_config() {
        let dir = std::env::temp_dir().join("lunaris-config-test-layout");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-layout.toml");
        std::fs::write(
            &path,
            r#"
[layout]
inner_gap = 4
outer_gap = 12
smart_gaps = false

[[layout.window_rules]]
match = { app_id = "pavucontrol" }
action = "float"

[[layout.window_rules]]
match = { app_id = "firefox", title = ".*Picture-in-Picture.*" }
action = "float"
"#,
        )
        .unwrap();

        let tc = load_toml_config(&path);
        assert_eq!(tc.layout.inner_gap, 4);
        assert_eq!(tc.layout.outer_gap, 12);
        assert!(!tc.layout.smart_gaps);
        assert_eq!(tc.layout.window_rules.len(), 2);
        assert_eq!(tc.layout.window_rules[0].action, WindowAction::Float);
        assert!(tc.layout.window_rules[0].matcher.app_id.as_ref().unwrap().is_match("pavucontrol"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_load_keybindings() {
        let dir = std::env::temp_dir().join("lunaris-config-test-kb");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-keybindings.toml");
        std::fs::write(
            &path,
            r#"
[keybindings]
"Super+T" = "toggle_tiling"
"Super+Shift+H" = "move_left"
"Super+Ctrl+L" = "resize_grow_width"
"#,
        )
        .unwrap();

        let tc = load_toml_config(&path);
        assert_eq!(tc.keybindings.len(), 3);

        let toggle = tc.keybindings.iter().find(|k| k.action == "toggle_tiling").unwrap();
        assert!(toggle.modifiers.super_key);
        assert!(!toggle.modifiers.shift);
        assert_eq!(toggle.key, "T");

        let move_left = tc.keybindings.iter().find(|k| k.action == "move_left").unwrap();
        assert!(move_left.modifiers.super_key);
        assert!(move_left.modifiers.shift);
        assert_eq!(move_left.key, "H");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_window_match() {
        let match_all = WindowMatch { app_id: None, title: None, window_type: None };
        assert!(match_all.matches("any", "any", false));

        let match_app = WindowMatch {
            app_id: Some(regex::Regex::new("pavucontrol").unwrap()),
            title: None,
            window_type: None,
        };
        assert!(match_app.matches("pavucontrol", "Volume Control", false));
        assert!(!match_app.matches("firefox", "Mozilla", false));

        let match_both = WindowMatch {
            app_id: Some(regex::Regex::new("firefox").unwrap()),
            title: Some(regex::Regex::new(".*PiP.*").unwrap()),
            window_type: None,
        };
        assert!(match_both.matches("firefox", "PiP Window", false));
        assert!(!match_both.matches("firefox", "Normal Page", false));

        let match_dialog = WindowMatch {
            app_id: None,
            title: None,
            window_type: Some("dialog".into()),
        };
        assert!(match_dialog.matches("any", "any", true));
        assert!(!match_dialog.matches("any", "any", false));
    }

    // ── Keybinding fragments ───────────────────────────────────────────

    #[test]
    fn load_keybinding_fragments_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fragments = load_keybinding_fragments(dir.path());
        assert!(fragments.is_empty());
    }

    #[test]
    fn load_keybinding_fragments_missing_dir_is_ok() {
        let dir = std::env::temp_dir().join("lunaris-does-not-exist-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_keybinding_fragments(&dir).is_empty());
    }

    #[test]
    fn load_keybinding_fragments_reads_all_tomls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("com.example.a.toml"),
            "[keybindings]\n\"Super+A\" = \"module:com.example.a:open\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("com.example.b.toml"),
            "[keybindings]\n\"Super+B\" = \"module:com.example.b:activate\"\n\
             \"Ctrl+B\" = \"module:com.example.b:secondary\"\n",
        )
        .unwrap();
        // Non-TOML must be ignored.
        std::fs::write(dir.path().join("README.md"), "ignore me").unwrap();

        let mut fragments = load_keybinding_fragments(dir.path());
        fragments.sort_by(|a, b| a.binding.cmp(&b.binding));
        assert_eq!(fragments.len(), 3);
        assert!(fragments
            .iter()
            .any(|e| e.binding == "Super+A" && e.module_id == "com.example.a"));
        assert!(fragments
            .iter()
            .filter(|e| e.module_id == "com.example.b")
            .count()
            == 2);
    }

    #[test]
    fn load_keybinding_fragments_skips_malformed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.toml"), "not valid toml = = =").unwrap();
        std::fs::write(
            dir.path().join("ok.toml"),
            "[keybindings]\n\"Super+X\" = \"module:ok:x\"\n",
        )
        .unwrap();
        let fragments = load_keybinding_fragments(dir.path());
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].module_id, "ok");
    }

    #[test]
    fn keybinding_fragment_dir_is_sibling_of_toml() {
        let toml = std::path::Path::new("/etc/lunaris/compositor.toml");
        let frag = keybinding_fragment_dir(toml);
        assert_eq!(
            frag,
            std::path::Path::new("/etc/lunaris/compositor.d/keybindings.d")
        );
    }

    /// Round-trip: serialize a `LunarisRuntimeState`, parse it back,
    /// and confirm the fields survive. Catches accidental serde
    /// drift on the runtime-state schema.
    #[test]
    fn runtime_state_round_trips_through_toml() {
        let original = LunarisRuntimeState {
            autotile: Some(true),
            pinned_workspaces: Some(Vec::new()),
        };
        let serialized = lunaris_runtime_serialize(&original).expect("serialize");
        let parsed: LunarisRuntimeState =
            toml::from_str(&serialized).expect("parse round-trip");
        assert_eq!(parsed.autotile, original.autotile);
        assert_eq!(
            parsed.pinned_workspaces.as_ref().map(|v| v.len()),
            Some(0)
        );
    }

    /// Missing fields stay `None`. The override semantics in
    /// `apply_runtime_state_overrides` then leave the TOML value
    /// untouched, so a partially-populated state file can never
    /// silently revert a TOML setting it doesn't mention.
    #[test]
    fn runtime_state_missing_fields_use_defaults() {
        let parsed: LunarisRuntimeState =
            toml::from_str("").expect("empty body parses");
        assert_eq!(
            parsed.autotile, None,
            "missing autotile must be None so TOML wins"
        );
        assert_eq!(
            parsed.pinned_workspaces, None,
            "missing pinned_workspaces must be None so TOML wins"
        );

        // Partial: only autotile present.
        let parsed: LunarisRuntimeState =
            toml::from_str("autotile = true").expect("partial body parses");
        assert_eq!(parsed.autotile, Some(true));
        assert_eq!(parsed.pinned_workspaces, None);
    }

    /// `load_runtime_state` returns defaults (all-`None`) when the
    /// file is missing. Combined with `apply_runtime_state_overrides`,
    /// this means a fresh install never overwrites the user's TOML
    /// with hardcoded false/empty defaults.
    #[test]
    fn load_runtime_state_missing_path_is_default() {
        let state = Config::load_runtime_state(&None);
        assert_eq!(state.autotile, None);
        assert_eq!(state.pinned_workspaces, None);

        // Path that doesn't exist on disk.
        let nonexistent = Some(std::path::PathBuf::from(
            "/tmp/nonexistent-lunaris-runtime-state.toml",
        ));
        let state = Config::load_runtime_state(&nonexistent);
        assert_eq!(state.autotile, None);
    }

    /// `[system_actions]` parses known keys onto the System enum
    /// and skips unknown keys with a warning rather than crashing.
    #[test]
    fn parse_system_actions_known_and_unknown() {
        use shortcuts::action::System;
        let toml: toml::Table = toml::from_str(
            r#"
[system_actions]
VolumeRaise = "spawn:wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+"
BrightnessUp = "shell:brightness_up"
NotARealAction = "spawn:nope"
"#,
        )
        .unwrap();
        let actions = parse_system_actions(&toml);
        assert_eq!(
            actions.get(&System::VolumeRaise),
            Some(&"spawn:wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+".to_string())
        );
        assert_eq!(
            actions.get(&System::BrightnessUp),
            Some(&"shell:brightness_up".to_string())
        );
        // Unknown variant is silently dropped (logged warn at runtime).
        assert_eq!(actions.len(), 2);
    }

    /// Defaults must cover the everyday Fn-row keys (volume,
    /// brightness, mute, play/pause). A user with a bare
    /// compositor.toml and no `[system_actions]` should still be
    /// able to use those keys.
    #[test]
    fn default_system_actions_covers_fn_row() {
        use shortcuts::action::System;
        let defaults = default_system_actions();
        for action in [
            System::VolumeRaise,
            System::VolumeLower,
            System::Mute,
            System::BrightnessUp,
            System::BrightnessDown,
            System::PlayPause,
            System::PlayNext,
            System::PlayPrev,
        ] {
            assert!(
                defaults.contains_key(&action),
                "default missing for {action:?}"
            );
        }
    }

    /// Every default system-action value must start with one of
    /// the known dispatch prefixes (`shell:` or `spawn:`). The
    /// `Action::System` arm in `input/actions.rs` strips these
    /// prefixes before dispatch — a bare value would have been
    /// passed verbatim to `/bin/sh -c` and silently failed. This
    /// is the data-side guard against the regression flagged in
    /// compositor #29 review HIGH 1.
    #[test]
    fn default_system_actions_use_known_prefix() {
        for (action, command) in default_system_actions() {
            assert!(
                command.starts_with("shell:") || command.starts_with("spawn:"),
                "default for {action:?} = {command:?} must use a known prefix \
                 (shell: or spawn:); bare strings break system-action dispatch"
            );
        }
    }

    /// Cosmic-shape Shortcuts is built from our toml_keybindings.
    /// Cosmic-`Action` variants flow through, Lunaris-private
    /// actions (shell:, module:, scratchpad, etc.) are filtered
    /// out so only what the cosmic dispatch loops understand ends
    /// up in the table.
    #[test]
    fn build_cosmic_shortcuts_maps_only_cosmic_actions() {
        let kbs = vec![
            KeyBinding {
                modifiers: KeyBindingModifiers {
                    super_key: true,
                    shift: true,
                    ..Default::default()
                },
                key: "q".into(),
                action: "close_window".into(),
            },
            KeyBinding {
                modifiers: KeyBindingModifiers {
                    super_key: true,
                    ..Default::default()
                },
                key: "space".into(),
                action: "shell:waypointer_open".into(),
            },
            KeyBinding {
                modifiers: KeyBindingModifiers {
                    super_key: true,
                    ..Default::default()
                },
                key: "F".into(),
                action: "fullscreen".into(),
            },
        ];
        let shortcuts = build_cosmic_shortcuts(&kbs);
        // close_window + fullscreen are cosmic actions → in the table.
        // shell:waypointer_open is Lunaris-private → filtered out.
        assert_eq!(shortcuts.0.len(), 2);
        let actions: Vec<_> = shortcuts.0.values().collect();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, shortcuts::Action::Close)),
            "Close action missing from cosmic shortcuts"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, shortcuts::Action::Fullscreen)),
            "Fullscreen action missing from cosmic shortcuts"
        );
    }

    /// `format_keybinding` produces the canonical "Super+Shift+Q"
    /// shape used in the context-menu shortcut labels.
    #[test]
    fn format_keybinding_canonical_order() {
        let mods = KeyBindingModifiers {
            super_key: true,
            ctrl: true,
            alt: true,
            shift: true,
        };
        assert_eq!(format_keybinding(&mods, "Q"), "Super+Ctrl+Alt+Shift+Q");

        let mods_super_only = KeyBindingModifiers {
            super_key: true,
            ..Default::default()
        };
        assert_eq!(format_keybinding(&mods_super_only, "Tab"), "Super+Tab");

        let mods_none = KeyBindingModifiers::default();
        assert_eq!(format_keybinding(&mods_none, "F1"), "F1");
    }

    /// Override semantics: when a TOML key collides with a
    /// default, the user's value wins. This mirrors the Right-most
    /// merge order in `Config::load`.
    #[test]
    fn toml_overrides_beat_defaults_via_merge() {
        use shortcuts::action::System;
        let mut merged = default_system_actions();
        let toml: toml::Table = toml::from_str(
            r#"
[system_actions]
BrightnessUp = "spawn:my-custom-brightness-helper"
"#,
        )
        .unwrap();
        let overrides = parse_system_actions(&toml);
        for (k, v) in overrides {
            merged.insert(k, v);
        }
        assert_eq!(
            merged.get(&System::BrightnessUp),
            Some(&"spawn:my-custom-brightness-helper".to_string())
        );
        // Untouched defaults still present.
        assert!(merged.contains_key(&System::VolumeRaise));
    }

    /// A corrupt state file is removed and replaced with defaults
    /// rather than crashing the compositor.
    #[test]
    fn load_runtime_state_corrupt_file_resets() {
        let dir = std::env::temp_dir().join("lunaris-runtime-state-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("corrupt.toml");
        std::fs::write(&path, "not = valid = toml = at = all").unwrap();
        assert!(path.exists(), "test setup");

        let state = Config::load_runtime_state(&Some(path.clone()));
        assert_eq!(state.autotile, None, "fell back to defaults");
        assert!(
            !path.exists(),
            "corrupt file must be removed so the next write starts clean"
        );
    }

    /// Override precedence: an empty/missing runtime state must NOT
    /// overwrite a TOML-configured `autotile = true`. Codex review
    /// HIGH 2 — a fresh install used to revert this silently.
    #[test]
    fn runtime_state_none_does_not_override_toml() {
        let mut cosmic = CosmicCompConfig::default();
        cosmic.autotile = true;

        let runtime = LunarisRuntimeState::default(); // None / None
        apply_runtime_state_overrides(&mut cosmic, &runtime);

        assert!(
            cosmic.autotile,
            "TOML autotile=true must survive an empty runtime state"
        );
    }

    /// Override precedence: an explicit `Some(_)` in runtime state
    /// wins over the TOML value. Mirrors the post-toggle path
    /// where the user has actively flipped autotile.
    #[test]
    fn runtime_state_some_overrides_toml() {
        let mut cosmic = CosmicCompConfig::default();
        cosmic.autotile = false;

        let runtime = LunarisRuntimeState {
            autotile: Some(true),
            pinned_workspaces: None,
        };
        apply_runtime_state_overrides(&mut cosmic, &runtime);

        assert!(
            cosmic.autotile,
            "explicit Some(true) must override the TOML value"
        );
    }
}
