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
/// Corner radius **no longer lives here** — it moved to
/// `[overrides].radius_intensity` (semantic 0.0..=2.0 multiplier
/// applied to the active theme's chip/button/input/card/modal
/// scale). Gaps live in `compositor.toml [layout]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowSection {
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
    /// User-applied radius multiplier `0.0..=2.0`. Replaces the
    /// old `[window].corner_radius` (integer pixels) with a
    /// semantic, percentage-based knob — see
    /// `docs/architecture/theme-system.md` §4.
    #[serde(default)]
    pub radius_intensity: Option<f32>,
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
                    "appearance: loaded {} ({} bytes) -> mode={:?} accent_override={:?} radius_intensity={:?} bw={:?} focused={:?} unfocused={:?}",
                    path.display(),
                    contents.len(),
                    cfg.theme.mode.as_deref().or(cfg.theme.active.as_deref()),
                    cfg.overrides.accent,
                    cfg.overrides.radius_intensity,
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

/// Apply overrides from `appearance.toml` to a resolved theme.
/// Called both from the theme watcher (when theme.toml changes)
/// and the appearance watcher (when appearance.toml changes).
pub fn apply_to_theme(theme: &mut lunaris_theme::LunarisTheme, cfg: &AppearanceConfig) {
    let w = &cfg.window;

    // Radius intensity is the user knob; theme defines the absolute
    // base values per token. `effective_*()` applies this at emit
    // time on the consumer side (window-header rendering, CSS-var
    // injection). We just store it on the theme.
    if let Some(intensity) = cfg.overrides.radius_intensity {
        theme.radius.intensity = intensity;
        tracing::info!(
            "appearance: applied radius_intensity {intensity:.2} \
             -> effective: chip={} button={} card={} modal={} (full unscaled)",
            theme.effective_chip(),
            theme.effective_button(),
            theme.effective_card(),
            theme.effective_modal(),
        );
    }

    if let Some(bw) = w.border_width {
        theme.wm.active_hint = bw;
        tracing::debug!("appearance: applied border_width {bw}");
    }

    // Focus border color. Explicit user setting wins; otherwise follow
    // the user's `[overrides].accent` if they configured one; otherwise
    // default to pure white on dark / pure black on light. The render
    // pipeline multiplies by `IndicatorShader::FOCUS_BORDER_ALPHA`
    // (currently 0.13) so the final pixel is `rgba(255,255,255,0.13)`
    // on dark and `rgba(0,0,0,0.13)` on light — a subtle monochrome
    // outline, never a saturated accent ring. Using the foreground
    // tokens #fafafa/#171717 here would shift the math slightly (and
    // make the light-mode border cast a dark-grey tint), so we pin to
    // pure 0/1 channel values for the common case.
    let focused_rgb = if let Some(ref color) = w.border.focused {
        resolve_focused(color, cfg, theme)
    } else if cfg.overrides.accent.is_some() {
        Some(effective_accent(cfg, theme))
    } else {
        let dark = cfg
            .theme
            .mode
            .as_deref()
            .or(cfg.theme.active.as_deref())
            .unwrap_or("dark")
            != "light";
        Some(if dark {
            [1.0, 1.0, 1.0]
        } else {
            [0.0, 0.0, 0.0]
        })
    };
    match focused_rgb {
        Some(rgb) => {
            theme.wm.window_hint = Some([rgb[0], rgb[1], rgb[2], 1.0]);
            tracing::info!(
                "appearance: window_hint <- {:?} (from {:?})",
                theme.wm.window_hint,
                w.border.focused.as_deref().unwrap_or("<default:$accent>"),
            );
        }
        None => {
            tracing::warn!(
                "appearance: unrecognised [window.border].focused = {:?}",
                w.border.focused
            );
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

    // Re-compose the theme through the SAME path the compositor
    // uses at startup (`crate::theme::compose_effective_theme` via
    // the `replace_lunaris_theme` setter below). That path picks
    // the Lunaris Dark / Light preset from `[theme] mode`, merges
    // `theme.toml` on top, and THEN applies the appearance.toml
    // overrides. Previously this function took the
    // `lunaris_theme::LunarisTheme::load()` shortcut — which
    // silently fell through to the Panda preset when `theme.toml`
    // was empty, producing a Panda-coloured window header whenever
    // the user touched `appearance.toml`. See the Theme Integration
    // bug ticket attached to this change for the reproduction.
    let theme = crate::theme::recompose_effective_theme();

    tracing::info!(
        "appearance: composed theme — \
         mode={:?} variant={:?} \
         bg_shell={:?} bg_card={:?} fg_primary={:?} accent={:?} \
         border={:?} error={:?} \
         radius_intensity={} effective_chip={} effective_button={} \
         effective_card={} effective_modal={} \
         window_corners={:?} active_hint={} window_hint={:?} \
         font_sans={:?}",
        cfg.theme.mode.as_deref().or(cfg.theme.active.as_deref()),
        theme.meta.variant,
        theme.color.bg_shell, theme.color.bg_card,
        theme.color.fg_primary, theme.color.accent,
        theme.color.border_default, theme.color.error,
        theme.radius.intensity,
        theme.effective_chip(), theme.effective_button(),
        theme.effective_card(), theme.effective_modal(),
        theme.radius.window_corners,
        theme.wm.active_hint, theme.wm.window_hint,
        theme.typography.font_sans,
    );

    crate::theme::replace_lunaris_theme(theme.clone());
    state.common.lunaris_theme = theme.clone();
    {
        let mut shell = state.common.shell.write();
        shell.lunaris_theme = theme;
    }

    // Feature 4-C: window-header rasteriser keeps a per-window
    // `MemoryRenderBuffer` cache keyed on a theme-generation
    // counter. Bump it so every cached pixmap re-rasterises on
    // the next frame with the new colours / radii. Without this
    // a theme edit would only visibly apply to windows that
    // happen to change their visual state (title change, hover,
    // etc.) before the next user action.
    crate::backend::render::window_header::bump_theme_generation();

    // Gaps are NOT handled here — they live in compositor.toml [layout]
    // and are managed by the existing compositor.toml watcher.

    // Schedule re-render on all outputs so radius/border width changes show.
    let outputs: Vec<_> = state.common.shell.read().outputs().cloned().collect();
    tracing::info!("appearance: schedule_render on {} outputs", outputs.len());
    for output in outputs {
        state.backend.schedule_render(&output);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Hex parsing ──────────────────────────────────────────────────

    #[test]
    fn test_hex_parse_rgb_short() {
        let rgb = parse_hex_rgb("#fff").unwrap();
        assert!((rgb[0] - 1.0).abs() < 0.01, "r should be 1.0");
        assert!((rgb[1] - 1.0).abs() < 0.01, "g should be 1.0");
        assert!((rgb[2] - 1.0).abs() < 0.01, "b should be 1.0");
    }

    #[test]
    fn test_hex_parse_rrggbb() {
        let rgb = parse_hex_rgb("#6366f1").unwrap();
        assert!((rgb[0] - 99.0 / 255.0).abs() < 0.01, "r");
        assert!((rgb[1] - 102.0 / 255.0).abs() < 0.01, "g");
        assert!((rgb[2] - 241.0 / 255.0).abs() < 0.01, "b");
    }

    #[test]
    fn test_hex_parse_rrggbbaa_ignores_alpha() {
        let rgb = parse_hex_rgb("#6366f180").unwrap();
        assert!((rgb[0] - 99.0 / 255.0).abs() < 0.01, "r same as without alpha");
    }

    #[test]
    fn test_hex_parse_invalid() {
        assert!(parse_hex_rgb("not-hex").is_none());
        assert!(parse_hex_rgb("#xyz").is_none());
        assert!(parse_hex_rgb("#12345").is_none());
        assert!(parse_hex_rgb("").is_none());
    }

    // ── Config parsing ───────────────────────────────────────────────

    #[test]
    fn test_parse_empty_string() {
        let cfg: AppearanceConfig = toml::from_str("").unwrap();
        assert!(cfg.window.border_width.is_none());
        assert!(cfg.window.border.focused.is_none());
        assert!(cfg.overrides.radius_intensity.is_none());
    }

    #[test]
    fn test_parse_radius_intensity() {
        let cfg: AppearanceConfig = toml::from_str(
            "[overrides]\nradius_intensity = 1.5\n",
        )
        .unwrap();
        assert_eq!(cfg.overrides.radius_intensity, Some(1.5));
    }

    #[test]
    fn test_parse_border_sentinels() {
        let cfg: AppearanceConfig = toml::from_str(
            "[window.border]\nfocused = \"$accent\"\nunfocused = \"$border\"\n",
        )
        .unwrap();
        assert_eq!(cfg.window.border.focused.as_deref(), Some("$accent"));
        assert_eq!(cfg.window.border.unfocused.as_deref(), Some("$border"));
    }

    #[test]
    fn test_parse_theme_and_overrides() {
        let cfg: AppearanceConfig = toml::from_str(
            "[theme]\nactive = \"light\"\nmode = \"light\"\n\n[overrides]\naccent = \"$foreground\"\n",
        )
        .unwrap();
        assert_eq!(cfg.theme.mode.as_deref(), Some("light"));
        assert_eq!(cfg.overrides.accent.as_deref(), Some("$foreground"));
    }

    // ── apply_to_theme ───────────────────────────────────────────────

    /// Builds a sane test theme from the bundled dark.toml bytes.
    /// We use a minimal-meta document; the resolver fills sane
    /// defaults for everything else.
    fn test_theme() -> lunaris_theme::LunarisTheme {
        let bytes = r##"
[meta]
id = "dark"
name = "Lunaris Dark"
variant = "dark"
"##;
        lunaris_theme::LunarisTheme::from_bundled(bytes).unwrap()
    }

    #[test]
    fn intensity_default_value_is_1() {
        let theme = test_theme();
        assert_eq!(theme.radius.intensity, 1.0);
    }

    #[test]
    fn intensity_override_applies() {
        let mut theme = test_theme();
        let cfg: AppearanceConfig =
            toml::from_str("[overrides]\nradius_intensity = 1.5\n").unwrap();
        apply_to_theme(&mut theme, &cfg);
        assert_eq!(theme.radius.intensity, 1.5);
    }

    #[test]
    fn intensity_zero_yields_sharp_effective_radii() {
        let mut theme = test_theme();
        let cfg: AppearanceConfig =
            toml::from_str("[overrides]\nradius_intensity = 0.0\n").unwrap();
        apply_to_theme(&mut theme, &cfg);
        assert_eq!(theme.effective_chip(),   0.0);
        assert_eq!(theme.effective_button(), 0.0);
        assert_eq!(theme.effective_card(),   0.0);
        assert_eq!(theme.effective_modal(),  0.0);
        // Full + window_corners NEVER scaled.
        assert_eq!(theme.effective_full(), 9999.0);
    }

    #[test]
    fn intensity_max_doubles_effective_radii() {
        let mut theme = test_theme();
        let cfg: AppearanceConfig =
            toml::from_str("[overrides]\nradius_intensity = 2.0\n").unwrap();
        apply_to_theme(&mut theme, &cfg);
        // chip(4) * 2.0 = 8
        assert_eq!(theme.effective_chip(), 8.0);
        // button(6) * 2.0 = 12
        assert_eq!(theme.effective_button(), 12.0);
    }

    #[test]
    fn test_apply_border_width() {
        let mut theme = test_theme();
        let cfg: AppearanceConfig = toml::from_str("[window]\nborder_width = 3\n").unwrap();
        apply_to_theme(&mut theme, &cfg);
        assert_eq!(theme.wm.active_hint, 3);
    }

    // ── Sentinel resolution ──────────────────────────────────────────

    #[test]
    fn test_effective_accent_foreground_dark() {
        let cfg: AppearanceConfig = toml::from_str(
            "[theme]\nmode = \"dark\"\n\n[overrides]\naccent = \"$foreground\"\n",
        )
        .unwrap();
        let theme = test_theme();
        let rgb = effective_accent(&cfg, &theme);
        // MONO_DARK = #fafafa → ~0.98
        assert!(rgb[0] > 0.95, "dark monochrome should be near-white: {}", rgb[0]);
    }

    #[test]
    fn test_effective_accent_foreground_light() {
        let cfg: AppearanceConfig = toml::from_str(
            "[theme]\nmode = \"light\"\n\n[overrides]\naccent = \"$foreground\"\n",
        )
        .unwrap();
        let theme = test_theme();
        let rgb = effective_accent(&cfg, &theme);
        // MONO_LIGHT = #171717 → ~0.09
        assert!(rgb[0] < 0.15, "light monochrome should be near-black: {}", rgb[0]);
    }

    #[test]
    fn test_effective_accent_hex_override() {
        let cfg: AppearanceConfig = toml::from_str(
            "[overrides]\naccent = \"#ff0000\"\n",
        )
        .unwrap();
        let theme = test_theme();
        let rgb = effective_accent(&cfg, &theme);
        assert!((rgb[0] - 1.0).abs() < 0.01, "red channel");
        assert!(rgb[1] < 0.01, "green channel");
    }

    #[test]
    fn test_effective_accent_no_override_falls_back_to_theme() {
        let cfg: AppearanceConfig = toml::from_str("").unwrap();
        let theme = test_theme();
        let rgb = effective_accent(&cfg, &theme);
        let expected = theme.accent_rgb();
        assert_eq!(rgb, expected);
    }
}
