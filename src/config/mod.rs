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

mod input_config;
pub mod key_bindings;
mod types;

pub use cosmic_comp_config::EdidProduct;
use cosmic_comp_config::{
    CosmicCompConfig, XkbConfig,
    input::{DeviceState as InputDeviceState, InputConfig, TouchpadOverride},
    output::comp::{
        OutputConfig, OutputInfo, OutputState, OutputsConfig, TransformDef, load_outputs,
    },
};
pub use key_bindings::{Action, PrivateAction, action_from_str, keysym_from_str};
use types::WlXkbConfig;

#[derive(Debug)]
pub struct Config {
    pub dynamic_conf: DynamicConfig,
    /// Path to the Lunaris TOML compositor config file.
    pub toml_path: PathBuf,
    /// Legacy cosmic-config handle, used by zoom.rs for writing config back.
    /// Will be removed when zoom.rs is replaced.
    pub cosmic_helper: cosmic_config::Config,
    /// Compositor configuration loaded from TOML.
    pub cosmic_conf: CosmicCompConfig,
    /// cosmic-config context for `com.system76.CosmicSettings.Shortcuts`
    pub settings_context: cosmic_config::Config,
    /// Key bindings from `com.system76.CosmicSettings.Shortcuts`
    pub shortcuts: Shortcuts,
    // Tiling exceptions from `com.system76.CosmicSettings.WindowRules`
    pub tiling_exceptions: Vec<ApplicationException>,
    /// Layout and tiling configuration from TOML.
    pub layout: LayoutConfig,
    /// Keybindings from TOML `[keybindings]` section.
    pub toml_keybindings: Vec<KeyBinding>,
    /// System actions from `com.system76.CosmicSettings.Shortcuts`
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
}

#[derive(Default, Debug, Deserialize, Serialize)]
pub struct NumlockStateConfig {
    pub last_state: bool,
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
}

impl ScreenFilter {
    pub fn is_noop(&self) -> bool {
        !self.inverted && self.color_filter.is_none()
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
    /// Window rules for float/tile decisions.
    pub window_rules: Vec<WindowRule>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            inner_gap: 8,
            outer_gap: 8,
            smart_gaps: true,
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
fn parse_keybinding(binding: &str) -> Option<(KeyBindingModifiers, String)> {
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
}

fn load_toml_config(path: &std::path::Path) -> TomlConfig {
    let default = || TomlConfig {
        cosmic: CosmicCompConfig::default(),
        layout: LayoutConfig::default(),
        keybindings: Vec::new(),
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
    if let Some(xkb) = table.get("xkb_config").and_then(|v| v.as_table()) {
        if let Some(s) = xkb.get("layout").and_then(|v| v.as_str()) {
            config.xkb_config.layout = s.to_string();
        }
        if let Some(s) = xkb.get("model").and_then(|v| v.as_str()) {
            config.xkb_config.model = s.to_string();
        }
        if let Some(s) = xkb.get("variant").and_then(|v| v.as_str()) {
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

    tracing::info!(
        "loaded compositor config from {} (xkb layout={:?})",
        path.display(),
        config.xkb_config.layout,
    );

    if config.xkb_config.layout.is_empty() {
        tracing::warn!("XKB layout is EMPTY after loading TOML from {}", path.display());
    }

    let layout = parse_layout_config(&table);
    let keybindings = parse_keybindings_config(&table);

    TomlConfig {
        cosmic: config,
        layout,
        keybindings,
    }
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
        "layout config: inner_gap={} outer_gap={} smart_gaps={} rules={}",
        layout.inner_gap,
        layout.outer_gap,
        layout.smart_gaps,
        layout.window_rules.len(),
    );

    layout
}

/// Default keybindings used when no `[keybindings]` section is present.
fn default_keybindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    let defaults = [
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

        // Source key bindings from com.system76.CosmicSettings.Shortcuts
        let settings_context = shortcuts::context().expect("Failed to load shortcuts config");
        let system_actions = shortcuts::system_actions(&settings_context);
        let shortcuts = shortcuts::shortcuts(&settings_context);

        // Listen for updates to the keybindings config.
        match cosmic_config::calloop::ConfigWatchSource::new(&settings_context) {
            Ok(source) => {
                if let Err(err) = loop_handle.insert_source(source, |(config, keys), (), state| {
                    for key in keys {
                        match key.as_str() {
                            // Reload the keyboard shortcuts config.
                            "custom" | "defaults" => {
                                state.common.config.shortcuts = shortcuts::shortcuts(&config);
                            }

                            "system_actions" => {
                                state.common.config.system_actions =
                                    shortcuts::system_actions(&config);
                            }

                            _ => (),
                        }
                    }
                }) {
                    warn!(
                        ?err,
                        "Failed to watch com.system76.CosmicSettings.Shortcuts config"
                    );
                }
            }
            Err(err) => warn!(
                ?err,
                "failed to create config watch source for com.system76.CosmicSettings.Shortcuts"
            ),
        };

        let window_rules_context =
            window_rules::context().expect("Failed to load window rules config");
        let tiling_exceptions = window_rules::tiling_exceptions(&window_rules_context);

        match cosmic_config::calloop::ConfigWatchSource::new(&window_rules_context) {
            Ok(source) => {
                if let Err(err) = loop_handle.insert_source(source, |(config, keys), (), state| {
                    for key in keys {
                        match key.as_str() {
                            "tiling_exception_defaults" | "tiling_exception_custom" => {
                                let new_exceptions = window_rules::tiling_exceptions(&config);
                                state.common.config.tiling_exceptions = new_exceptions;
                                state.common.shell.write().update_tiling_exceptions(
                                    state.common.config.tiling_exceptions.iter(),
                                );
                            }
                            _ => (),
                        }
                    }
                }) {
                    warn!(
                        ?err,
                        "Failed to watch com.system76.CosmicSettings.WindowRules config"
                    );
                }
            }
            Err(err) => warn!(
                ?err,
                "failed to create config watch source for com.system76.CosmicSettings.WindowRules"
            ),
        };

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

        // Legacy cosmic-config handle kept for zoom.rs write-back.
        let cosmic_helper =
            cosmic_config::Config::new("com.system76.CosmicComp", 1).unwrap();

        Config {
            dynamic_conf: Self::load_dynamic(&xdg),
            cosmic_conf: cosmic_comp_config,
            toml_path,
            cosmic_helper,
            settings_context,
            shortcuts,
            tiling_exceptions,
            layout: layout_config,
            toml_keybindings,
            system_actions,
            nested: false,
        }
    }

    fn load_dynamic(xdg: &xdg::BaseDirectories) -> DynamicConfig {
        let output_path = xdg.place_state_file("cosmic-comp/outputs.ron").ok();
        let outputs = load_outputs(output_path.as_ref());
        let numlock_path = xdg.place_state_file("cosmic-comp/numlock.ron").ok();
        let numlock = Self::load_numlock(&numlock_path);

        let filter_path = xdg
            .place_state_file("cosmic-comp/a11y_screen_filter.ron")
            .ok();
        let filter = Self::load_filter_state(&filter_path);

        DynamicConfig {
            outputs: (output_path, outputs),
            numlock: (numlock_path, numlock),
            accessibility_filter: (filter_path, filter),
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
        }
    }

    pub fn shortcut_for_action(&self, action: &shortcuts::Action) -> Option<String> {
        self.shortcuts.shortcut_for_action(action)
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

pub struct PersistenceGuard<'a, T: Serialize>(Option<PathBuf>, &'a mut T);

impl<T: Serialize> std::ops::Deref for PersistenceGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.1
    }
}

impl<T: Serialize> std::ops::DerefMut for PersistenceGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.1
    }
}

impl<T: Serialize> Drop for PersistenceGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let content = match ron::ser::to_string_pretty(&self.1, Default::default()) {
                Ok(content) => content,
                Err(err) => {
                    warn!("Failed to serialize: {:?}", err);
                    return;
                }
            };

            let mut writer = match OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)
            {
                Ok(writer) => writer,
                Err(err) => {
                    warn!(?err, "Failed to persist {}.", path.display());
                    return;
                }
            };

            if let Err(err) = writer.write_all(content.as_bytes()) {
                warn!(?err, "Failed to persist {}", path.display());
            } else {
                let _ = writer.flush();
            }
        }
    }
}

impl DynamicConfig {
    pub fn outputs(&self) -> &OutputsConfig {
        &self.outputs.1
    }

    pub fn outputs_mut(&mut self) -> PersistenceGuard<'_, OutputsConfig> {
        PersistenceGuard(self.outputs.0.clone(), &mut self.outputs.1)
    }

    pub fn numlock(&self) -> &NumlockStateConfig {
        &self.numlock.1
    }

    pub fn numlock_mut(&mut self) -> PersistenceGuard<'_, NumlockStateConfig> {
        PersistenceGuard(self.numlock.0.clone(), &mut self.numlock.1)
    }

    pub fn screen_filter(&self) -> &ScreenFilter {
        &self.accessibility_filter.1
    }

    pub fn screen_filter_mut(&mut self) -> PersistenceGuard<'_, ScreenFilter> {
        PersistenceGuard(
            self.accessibility_filter.0.clone(),
            &mut self.accessibility_filter.1,
        )
    }
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
    let new = toml.cosmic;

    // Update layout config and keybindings.
    state.common.config.layout = toml.layout;
    state.common.config.toml_keybindings = toml.keybindings;

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
}
