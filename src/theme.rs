// Lunaris theme watcher: reads ~/.config/lunaris/theme.toml and updates a global.

use calloop::LoopHandle;
use std::sync::RwLock;

use crate::state::State;

static LUNARIS_THEME: RwLock<Option<lunaris_theme::LunarisTheme>> = RwLock::new(None);

/// Read the global LunarisTheme. Falls back to Panda if not yet initialized.
pub fn lunaris_theme() -> lunaris_theme::LunarisTheme {
    LUNARIS_THEME
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(lunaris_theme::LunarisTheme::panda)
}

/// Update the global LunarisTheme.
fn set_lunaris_theme(theme: lunaris_theme::LunarisTheme) {
    *LUNARIS_THEME.write().unwrap() = Some(theme);
}

/// Public setter used by the appearance watcher after composing the
/// effective theme (theme.toml base + appearance.toml overrides).
/// Kept separate from the private `set_lunaris_theme` to make the
/// call-site intent explicit.
pub fn replace_lunaris_theme(theme: lunaris_theme::LunarisTheme) {
    set_lunaris_theme(theme);
}

/// Active window hint color from LunarisTheme as [r, g, b].
pub(crate) fn lunaris_hint_rgb(lt: &lunaris_theme::LunarisTheme) -> [f32; 3] {
    if let Some(hint) = lt.window_hint {
        [hint[0], hint[1], hint[2]]
    } else {
        lt.accent_rgb()
    }
}

/// Compose a fresh theme.toml load with any current appearance.toml
/// overrides. This is the single source of the "effective" theme used
/// by the compositor render paths.
///
/// The base preset is chosen from `appearance.toml [theme] mode`,
/// NOT from `lunaris_theme::LunarisTheme::load()` alone: if the
/// user's `theme.toml` is empty or missing, we must still land on
/// the canonical Lunaris dark/light palette (matching the shell's
/// `dark.toml` / `light.toml`) rather than fall through to the
/// historical Panda defaults — otherwise the compositor-rendered
/// window header (Feature 4-C) renders in a completely different
/// palette from the rest of the shell. See the Theme Integration
/// story attached to this ticket for why this was the #1 bug
/// reported after Feature 4-C landed.
/// Public wrapper around `compose_effective_theme()`. External
/// callers (currently the appearance watcher) rebuild the theme
/// through this entry so the compose pipeline stays single-source.
pub fn recompose_effective_theme() -> lunaris_theme::LunarisTheme {
    compose_effective_theme()
}

fn compose_effective_theme() -> lunaris_theme::LunarisTheme {
    let appearance = crate::config::appearance::current_appearance();

    // Pick the preset base. Default to dark — matches the Shell's
    // default `:root { color-scheme: dark }`.
    let mode_is_light = appearance
        .as_ref()
        .and_then(|a| a.theme.mode.as_deref().or(a.theme.active.as_deref()))
        .map(|m| m.eq_ignore_ascii_case("light"))
        .unwrap_or(false);
    let preset = if mode_is_light {
        lunaris_theme::LunarisTheme::lunaris_light()
    } else {
        lunaris_theme::LunarisTheme::lunaris_dark()
    };

    // Compose: user's theme.toml overrides (if any) on top of the
    // preset. This way an empty / missing theme.toml leaves the
    // Lunaris preset intact. `from_file_with_base` treats every
    // field as optional so partial theme.toml files merge cleanly.
    let theme_path = lunaris_theme::LunarisTheme::default_path();
    let base = match std::fs::read_to_string(&theme_path) {
        Ok(contents) => match toml::from_str::<lunaris_theme::LunarisThemeFile>(&contents) {
            Ok(file) => lunaris_theme::LunarisTheme::from_file_with_base(file, preset),
            Err(err) => {
                tracing::warn!(
                    "theme.toml parse error, using {} preset: {err}",
                    if mode_is_light { "lunaris_light" } else { "lunaris_dark" }
                );
                preset
            }
        },
        Err(_) => preset,
    };

    let mut composed = base;
    if let Some(overrides) = appearance {
        crate::config::appearance::apply_to_theme(&mut composed, &overrides);
    }

    tracing::info!(
        "theme: composed effective_theme (full) — \
         mode={} preset-bg_app={:?} final-bg_shell={:?} final-bg_app={:?} \
         final-fg_primary={:?} final-fg_secondary={:?} final-accent={:?} \
         final-border={:?} final-error={:?} \
         radius_sm={} radius_md={} radius_lg={} radius_window={:?} \
         active_hint={} font_sans={:?} font_weight_medium={}",
        if mode_is_light { "light" } else { "dark" },
        if mode_is_light {
            lunaris_theme::LunarisTheme::lunaris_light().bg_app
        } else {
            lunaris_theme::LunarisTheme::lunaris_dark().bg_app
        },
        composed.bg_shell, composed.bg_app,
        composed.fg_primary, composed.fg_secondary, composed.accent,
        composed.border, composed.error,
        composed.radius_sm, composed.radius_md, composed.radius_lg, composed.radius_s,
        composed.active_hint, composed.font_sans, composed.font_weight_medium,
    );
    composed
}

/// Initialize the global LunarisTheme and start a file watcher for live updates.
pub fn watch_theme(handle: LoopHandle<'_, State>) {
    // Initialize the global with the composed theme so the very first
    // frame already reflects both theme.toml and appearance.toml.
    set_lunaris_theme(compose_effective_theme());

    let (lt_ping_tx, lt_ping_rx) = calloop::ping::make_ping().unwrap();
    if let Err(e) = handle.insert_source(lt_ping_rx, move |_, _, state| {
        let lt = compose_effective_theme();
        set_lunaris_theme(lt.clone());
        state.common.lunaris_theme = lt.clone();
        let mut shell = state.common.shell.write();
        shell.lunaris_theme = lt;
        // Feature 4-C: the window-header renderer pulls
        // `lunaris_theme()` straight from this module but keeps a
        // per-window pixmap cache keyed on a generation counter;
        // bump it so every mapped window's header re-rasterises on
        // the next frame without us having to walk the list.
        crate::backend::render::window_header::bump_theme_generation();
    }) {
        tracing::error!("failed to insert lunaris theme ping source: {e}");
    }
    let lt_watcher = lunaris_theme::ThemeWatcher::start(move |_theme| {
        lt_ping_tx.ping();
    });
    match lt_watcher {
        Ok(w) => std::mem::forget(w),
        Err(e) => tracing::warn!("failed to start lunaris theme watcher: {e}"),
    }
}
