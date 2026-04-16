// SPDX-License-Identifier: GPL-3.0-only

//! Appearance config loader and watcher.
//!
//! Reads `~/.config/lunaris/appearance.toml` — the single-source config
//! file shared with the shell and the Settings app — and applies the
//! relevant `[window]` section to the compositor's effective theme.
//!
//! ## Scope
//!
//! * `[window].corner_radius`      -> `LunarisTheme::radius_s`
//! * `[window].border_width`       -> `LunarisTheme::active_hint`
//! * `[window].gap_inner / gap_outer / gap_smart` -> tiling layer gaps
//! * `[window.border].focused`    -> `LunarisTheme::window_hint`
//!   (accepts hex `#rrggbb[aa]` or the sentinel `"$accent"`)
//! * `[window.border].unfocused`  -> currently parsed but not rendered
//!   (Phase 4 render-loop patch; see project plan)
//!
//! ## Pipeline
//!
//! `theme.toml` remains the legacy source for everything the `SDK` theme
//! already exposes. When either file changes, we rebuild the effective
//! theme as:
//!
//! ```text
//! LunarisTheme::load()  // base, from theme.toml
//!     -> apply_to_theme(overrides)  // from appearance.toml
//!     -> set_lunaris_theme()        // global
//! ```
//!
//! That way a user who edits either file sees the same composed result,
//! without having to duplicate knowledge of both files across the code.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use calloop::LoopHandle;
use serde::Deserialize;

use crate::state::State;

/// Sentinel for `[window.border].focused` that binds the focused window
/// border colour to the effective accent. Same token the Settings app
/// writes when the user picks the "Use Accent" toggle.
pub const BORDER_FOCUSED_SENTINEL: &str = "$accent";

/// Sentinel for `[window.border].unfocused` that binds the unfocused
/// border colour to `theme.colors.border.default`. Parsed today, not
/// rendered until the tiling render loop gets per-node border support.
pub const BORDER_UNFOCUSED_SENTINEL: &str = "$border";

/// Sentinel value that can appear in `[overrides].accent` to request a
/// monochrome accent bound to the active theme mode. Mirrors the token
/// the Settings app writes when the user picks the "Monochrome" swatch.
pub const ACCENT_FOREGROUND_SENTINEL: &str = "$foreground";

/// Foreground hex per theme mode. Kept in sync with
/// `desktop-shell/src-tauri/themes/{dark,light}.toml [colors.foreground].primary`
/// and `app-settings/src/lib/stores/theme.ts` MONO_DARK/MONO_LIGHT.
const MONO_DARK: [f32; 3] = [0xfa as f32 / 255.0, 0xfa as f32 / 255.0, 0xfa as f32 / 255.0];
const MONO_LIGHT: [f32; 3] = [0x17 as f32 / 255.0, 0x17 as f32 / 255.0, 0x17 as f32 / 255.0];

/// Default appearance.toml path (`$XDG_CONFIG_HOME/lunaris/appearance.toml`),
/// falling back to `$HOME/.config/lunaris/appearance.toml`.
pub fn default_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("lunaris").join("appearance.toml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".config")
        .join("lunaris")
        .join("appearance.toml")
}

// ---------------------------------------------------------------------------
// Schema (sparse)
// ---------------------------------------------------------------------------

/// Parsed `[window]` section. Fields are optional so the parser degrades
/// gracefully on missing or partial configs.
///
/// Gaps are **not** in here any more — they live in `compositor.toml
/// [layout]` which already has a working watcher. Gap UI in the Settings
/// app writes there directly.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowSection {
    /// Corner radius in pixels. Integer — matches the Settings app
    /// slider, which only emits whole numbers. Using `f32` here would
    /// cause silent deserialization failures because TOML does not
    /// implicitly cast integers to floats.
    #[serde(default)]
    pub corner_radius: Option<u32>,
    #[serde(default)]
    pub border_width: Option<u32>,
    #[serde(default)]
    pub border: BorderSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BorderSection {
    #[serde(default)]
    pub focused: Option<String>,
    #[serde(default)]
    pub unfocused: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeSection {
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverridesSection {
    #[serde(default)]
    pub accent: Option<String>,
}

/// Top-level appearance.toml shape. Only the fields the compositor
/// cares about are parsed; other sections (fonts, accessibility, ...)
/// are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default)]
    pub theme: ThemeSection,
    #[serde(default)]
    pub overrides: OverridesSection,
    #[serde(default)]
    pub window: WindowSection,
}

impl AppearanceConfig {
    pub fn load_from(path: &Path) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(err) => {
                tracing::info!(
                    "appearance: no file at {} ({err}), using defaults",
                    path.display()
                );
                return Self::default();
            }
        };
        match toml::from_str::<Self>(&contents) {
            Ok(cfg) => {
                tracing::info!(
                    "appearance: loaded {} ({} bytes) -> mode={:?} accent_override={:?} radius={:?} bw={:?} focused={:?} unfocused={:?}",
                    path.display(),
                    contents.len(),
                    cfg.theme.mode.as_deref().or(cfg.theme.active.as_deref()),
                    cfg.overrides.accent,
                    cfg.window.corner_radius,
                    cfg.window.border_width,
                    cfg.window.border.focused,
                    cfg.window.border.unfocused,
                );
                cfg
            }
            Err(err) => {
                tracing::warn!(?err, "appearance: parse failed, using defaults");
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Global state + composition pipeline
// ---------------------------------------------------------------------------

static APPEARANCE: RwLock<Option<AppearanceConfig>> = RwLock::new(None);

/// Replace the current in-memory appearance overrides.
pub fn set_appearance(cfg: AppearanceConfig) {
    *APPEARANCE.write().unwrap() = Some(cfg);
}

/// Snapshot of the current overrides (clone). `None` if never loaded.
pub fn current_appearance() -> Option<AppearanceConfig> {
    APPEARANCE.read().unwrap().clone()
}

/// Apply overrides from `appearance.toml` to a fresh theme loaded from
/// `theme.toml`. Called both from the theme watcher (when theme.toml
/// changes) and from our own appearance watcher (when appearance.toml
/// changes) so the effective theme is always the composed result.
pub fn apply_to_theme(theme: &mut lunaris_theme::LunarisTheme, cfg: &AppearanceConfig) {
    let w = &cfg.window;

    if let Some(radius) = w.corner_radius {
        let r = radius as f32;
        theme.radius_s = [r, r, r, r];
        tracing::info!("appearance: applied radius {r}");
    }

    if let Some(bw) = w.border_width {
        theme.active_hint = bw;
        tracing::debug!("appearance: applied border_width {bw}");
    }

    if let Some(ref color) = w.border.focused {
        match resolve_focused(color, cfg, theme) {
            Some(rgb) => {
                theme.window_hint = Some([rgb[0], rgb[1], rgb[2], 1.0]);
                tracing::info!(
                    "appearance: window_hint <- {:?} (from {color:?})",
                    theme.window_hint
                );
            }
            None => {
                tracing::warn!(
                    "appearance: unrecognised [window.border].focused = {:?}",
                    color
                );
            }
        }
    }

    // border.unfocused is parsed but not applied — render loop patch
    // is deferred (Option b in the plan).
}

/// Resolve the effective accent colour the user sees everywhere else.
/// Honours `[overrides].accent` first (including `$foreground`), then
/// falls back to the legacy `LunarisTheme::accent_rgb()` from theme.toml.
pub fn effective_accent(cfg: &AppearanceConfig, theme: &lunaris_theme::LunarisTheme) -> [f32; 3] {
    if let Some(ref acc) = cfg.overrides.accent {
        if acc == ACCENT_FOREGROUND_SENTINEL {
            return monochrome_for_mode(&cfg.theme);
        }
        if let Some(rgb) = parse_hex_rgb(acc) {
            return rgb;
        }
    }
    theme.accent_rgb()
}

fn monochrome_for_mode(theme: &ThemeSection) -> [f32; 3] {
    let mode = theme
        .mode
        .as_deref()
        .or(theme.active.as_deref())
        .unwrap_or("dark");
    match mode {
        "light" => MONO_LIGHT,
        _ => MONO_DARK,
    }
}


// ---------------------------------------------------------------------------
// Colour resolution
// ---------------------------------------------------------------------------

fn resolve_focused(
    value: &str,
    cfg: &AppearanceConfig,
    theme: &lunaris_theme::LunarisTheme,
) -> Option<[f32; 3]> {
    if value == BORDER_FOCUSED_SENTINEL {
        return Some(effective_accent(cfg, theme));
    }
    parse_hex_rgb(value)
}

/// Parse a `#rgb`, `#rrggbb`, or `#rrggbbaa` string into linear [0,1] RGB.
/// Alpha is discarded.
fn parse_hex_rgb(hex: &str) -> Option<[f32; 3]> {
    let s = hex.strip_prefix('#')?;
    let (r, g, b) = match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            (r * 17, g * 17, b * 17)
        }
        6 | 8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            (r, g, b)
        }
        _ => return None,
    };
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

// ---------------------------------------------------------------------------
// Initial load + file watcher
// ---------------------------------------------------------------------------

/// Load `appearance.toml` once and install a notify watcher for live
/// reloads. Must be called during compositor startup, before the theme
/// watcher composes its first effective theme.
pub fn watch(loop_handle: LoopHandle<'_, State>) {
    let path = default_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Initial load.
    set_appearance(AppearanceConfig::load_from(&path));

    // Notify watcher -> calloop ping -> main-loop handler.
    let (tx, rx) = calloop::channel::channel::<()>();
    let watch_path = path.clone();
    let last_fire = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));

    let watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            tracing::debug!("appearance: watcher got error event");
            return;
        };
        use notify::EventKind;
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        let touches_target = event.paths.iter().any(|p| {
            p == &watch_path
                || p.file_name()
                    .map(|n| n == "appearance.toml")
                    .unwrap_or(false)
        });
        if !touches_target {
            return;
        }
        // 120ms debounce: atomic-rename bursts collapse into one reload.
        {
            let mut lf = last_fire.lock().unwrap();
            if lf.elapsed() < Duration::from_millis(120) {
                return;
            }
            *lf = Instant::now();
        }
        tracing::info!(
            "appearance: watcher fired ({:?}) on paths {:?}",
            event.kind,
            event.paths
        );
        let _ = tx.send(());
    }) {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(?err, "failed to create appearance.toml watcher");
            return;
        }
    };

    if let Some(parent) = path.parent() {
        use notify::Watcher;
        let mut w = watcher;
        if let Err(err) = w.watch(parent, notify::RecursiveMode::NonRecursive) {
            tracing::warn!(?err, "failed to watch appearance.toml parent directory");
            return;
        }
        // Leak the watcher so it stays alive for the process lifetime
        // (same pattern as theme.rs and compositor.toml watcher).
        std::mem::forget(w);
    }

    let reload_path = path;
    if let Err(err) = loop_handle.insert_source(rx, move |_, _, state| {
        // Small settle sleep before reading (atomic rename).
        std::thread::sleep(Duration::from_millis(30));
        handle_reload(&reload_path, state);
    }) {
        tracing::warn!(?err, "failed to insert appearance watcher source");
    }
}

/// Reload appearance.toml from disk and apply the new overrides to:
///   1. the composed LunarisTheme (radius, hint width, hint colour)
///   2. the tiling layer gaps on all workspaces (with recalculate)
///
/// Runs on the calloop main thread, so we can borrow `State` freely.
fn handle_reload(path: &Path, state: &mut State) {
    tracing::info!("appearance: handle_reload start");
    let cfg = AppearanceConfig::load_from(path);
    set_appearance(cfg.clone());

    // Re-compose the theme: start from theme.toml, apply appearance on top.
    let mut theme = lunaris_theme::LunarisTheme::load();
    apply_to_theme(&mut theme, &cfg);
    tracing::info!(
        "appearance: composed theme radius_s={:?} active_hint={} window_hint={:?}",
        theme.radius_s,
        theme.active_hint,
        theme.window_hint,
    );
    crate::theme::replace_lunaris_theme(theme.clone());
    state.common.lunaris_theme = theme.clone();
    {
        let mut shell = state.common.shell.write();
        shell.lunaris_theme = theme;
    }

    // Gaps are NOT handled here — they live in compositor.toml [layout]
    // and are managed by the existing compositor.toml watcher.

    // Schedule re-render on all outputs so radius/border width changes show.
    let outputs: Vec<_> = state.common.shell.read().outputs().cloned().collect();
    tracing::info!("appearance: schedule_render on {} outputs", outputs.len());
    for output in outputs {
        state.backend.schedule_render(&output);
    }
}
