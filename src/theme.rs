//! Lunaris theme integration for the compositor.
//!
//! Resolves `LunarisTheme` from the canonical bundled bytes
//! (`include_str!` cross-crate from desktop-shell/src-tauri/themes/)
//! merged with the user's `~/.config/lunaris/theme.toml` overlay,
//! then layered with `~/.config/lunaris/appearance.toml`
//! preferences (active theme id, accent override, radius
//! intensity, accessibility).
//!
//! The resolved theme is held in a process-wide `RwLock` and re-
//! resolved by the file watcher on any change to either of the
//! source files.
//!
//! See `docs/architecture/theme-system.md` for the SSoT layering
//! contract — this file is the compositor side of that contract.

use calloop::LoopHandle;
use std::sync::RwLock;

use crate::state::State;

/// Bundled bytes — same files the desktop-shell embeds so the two
/// binaries observe identical canonical defaults. Cross-crate
/// `include_str!` is the SSoT mechanism: a refactor that moves
/// the files breaks compile in BOTH crates immediately.
const DARK_TOML: &str =
    include_str!("../../desktop-shell/src-tauri/themes/dark.toml");
const LIGHT_TOML: &str =
    include_str!("../../desktop-shell/src-tauri/themes/light.toml");

static LUNARIS_THEME: RwLock<Option<lunaris_theme::LunarisTheme>> =
    RwLock::new(None);

/// Read the global LunarisTheme. Falls back to a freshly-resolved
/// dark theme if the watcher hasn't run yet (early startup
/// frames).
pub fn lunaris_theme() -> lunaris_theme::LunarisTheme {
    LUNARIS_THEME
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(default_dark_theme)
}

fn default_dark_theme() -> lunaris_theme::LunarisTheme {
    lunaris_theme::LunarisTheme::from_bundled(DARK_TOML)
        .expect("bundled dark.toml must parse — bundled bytes are static")
}

fn default_light_theme() -> lunaris_theme::LunarisTheme {
    lunaris_theme::LunarisTheme::from_bundled(LIGHT_TOML)
        .expect("bundled light.toml must parse — bundled bytes are static")
}

fn set_lunaris_theme(theme: lunaris_theme::LunarisTheme) {
    *LUNARIS_THEME.write().unwrap() = Some(theme);
}

/// Public setter used by the appearance watcher after composing
/// the effective theme. Kept distinct from the private setter so
/// the call-site intent is explicit.
pub fn replace_lunaris_theme(theme: lunaris_theme::LunarisTheme) {
    set_lunaris_theme(theme);
}

/// Active window hint color from LunarisTheme as `[r, g, b]`.
/// Falls back to the theme's accent if `[wm].window_hint` is unset.
pub(crate) fn lunaris_hint_rgb(lt: &lunaris_theme::LunarisTheme) -> [f32; 3] {
    if let Some(hint) = lt.wm.window_hint {
        [hint[0], hint[1], hint[2]]
    } else {
        lt.accent_rgb()
    }
}

/// Compose the effective theme from the bundled bytes + user
/// `theme.toml` overlay + `appearance.toml` preferences.
///
/// The base bundled bytes are picked from `appearance.toml
/// [theme].active` (or `mode` if active isn't set). User
/// `theme.toml` overlays via `LunarisTheme::resolve()`.
/// `appearance.toml` then layers radius_intensity + accent
/// override + reduce_motion on top.
pub fn recompose_effective_theme() -> lunaris_theme::LunarisTheme {
    let appearance = crate::config::appearance::current_appearance();

    // 1. Pick the bundled base from the user's `[theme].active`.
    //    Bundled ids match a TOML directly; non-bundled ids fall
    //    back to dark as the structural base and rely on the
    //    user-installed-theme overlay (step 2) to recolour.
    let active_id: String = appearance
        .as_ref()
        .and_then(|a| a.theme.active.as_deref().or(a.theme.mode.as_deref()))
        .unwrap_or("dark")
        .to_string();
    let bundled = match active_id.as_str() {
        "light" => LIGHT_TOML,
        _ => DARK_TOML,
    };

    // 2. User-installed-theme overlay if `theme.active` names a
    //    non-bundled id. Mirrors `desktop-shell::ThemeLoader::load`
    //    semantics so compositor + shell agree on which file is
    //    the active theme. (Codex post-Sprint review HIGH-2 fix.)
    let user_theme = if active_id != "dark" && active_id != "light" {
        let user_path = lunaris_theme::LunarisTheme::user_theme_path(&active_id);
        match std::fs::read_to_string(&user_path) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    "theme: active=`{active_id}` user-installed file missing at \
                     {} ({e}); falling back to bundled dark",
                    user_path.display()
                );
                None
            }
        }
    } else {
        None
    };

    // 3. Read user's `~/.config/lunaris/theme.toml` overlay if
    //    present.
    let custom_path = lunaris_theme::LunarisTheme::user_customization_path();
    let customization = std::fs::read_to_string(&custom_path).ok();

    // 4. `LunarisTheme::resolve()` merges bundled + user_theme +
    //    customization. On parse failure surface a warn and use
    //    the unmerged bundled bytes — we never want a malformed
    //    user file to block compositor startup.
    let mut composed = match lunaris_theme::LunarisTheme::resolve(
        bundled,
        user_theme.as_deref(),
        customization.as_deref(),
    ) {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(
                "theme: customization parse error, using bundled {active_id}: {err}"
            );
            match lunaris_theme::LunarisTheme::from_bundled(bundled) {
                Ok(t) => t,
                Err(_) => default_dark_theme(),
            }
        }
    };

    // 4. Apply appearance.toml preferences (accent override,
    //    radius_intensity, accessibility).
    if let Some(overrides) = appearance {
        crate::config::appearance::apply_to_theme(&mut composed, &overrides);
    }

    tracing::info!(
        "theme: composed (active={active_id} variant={:?}) \
         radius.chip={} radius.button={} radius.card={} \
         radius.intensity={} effective_card={} \
         active_hint={} font_sans={:?}",
        composed.meta.variant,
        composed.radius.chip,
        composed.radius.button,
        composed.radius.card,
        composed.radius.intensity,
        composed.effective_card(),
        composed.wm.active_hint,
        composed.typography.font_sans,
    );

    // Suppress unused-warning for default_light_theme during dev;
    // it's a public-API hook for future code paths.
    let _ = default_light_theme;

    composed
}

/// Start a file watcher for live theme updates. Watches both
/// `~/.config/lunaris/theme.toml` and
/// `~/.config/lunaris/appearance.toml`. The **initial** composition
/// is done by the caller before `State::new` (see `lib.rs`) so
/// frame 1 already has the correct theme; this function only
/// registers the runtime-change pipeline.
pub fn watch_theme(handle: LoopHandle<'_, State>) {
    let (lt_ping_tx, lt_ping_rx) = calloop::ping::make_ping().unwrap();
    if let Err(e) = handle.insert_source(lt_ping_rx, move |_, _, state| {
        let lt = recompose_effective_theme();
        set_lunaris_theme(lt.clone());
        state.common.lunaris_theme = lt.clone();
        let mut shell = state.common.shell.write();
        shell.lunaris_theme = lt;
        // Feature 4-C: window-header renderer pulls
        // `lunaris_theme()` directly but caches the rasterised
        // pixmap; bump generation so every window re-rasterises.
        crate::backend::render::window_header::bump_theme_generation();
    }) {
        tracing::error!("failed to insert lunaris theme ping source: {e}");
    }
    let lt_watcher = lunaris_theme::ThemeWatcher::start(move || {
        lt_ping_tx.ping();
    });
    match lt_watcher {
        Ok(w) => std::mem::forget(w),
        Err(e) => tracing::warn!("failed to start lunaris theme watcher: {e}"),
    }
}
