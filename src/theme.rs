// insert into the event loop, a watcher for the theme & theme mode for changes

// update a Arc<Mutex<Theme>> in the state on change of the theme and mark all interfaces for a redraw.

use calloop::LoopHandle;
use cosmic::cosmic_theme::{Theme, ThemeMode, palette};
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

pub(crate) fn _group_color(theme: &Theme) -> [f32; 3] {
    let neutral_8 = theme.palette.neutral_8;
    [neutral_8.red, neutral_8.green, neutral_8.blue]
}

pub(crate) fn active_window_hint(theme: &Theme) -> palette::Srgba {
    if let Some(hint) = theme.window_hint {
        palette::Srgba::from(hint)
    } else {
        theme.accent_color()
    }
}

/// Active window hint color from LunarisTheme as [r, g, b].
pub(crate) fn lunaris_hint_rgb(lt: &lunaris_theme::LunarisTheme) -> [f32; 3] {
    if let Some(hint) = lt.window_hint {
        [hint[0], hint[1], hint[2]]
    } else {
        lt.accent_rgb()
    }
}

pub fn watch_theme(handle: LoopHandle<'_, State>) -> Result<(), cosmic_config::Error> {
    let (ping_tx, ping_rx) = calloop::ping::make_ping().unwrap();
    let config_mode_helper = ThemeMode::config()?;
    let config_dark_helper = Theme::dark_config()?;
    let config_light_helper = Theme::light_config()?;

    if let Err(e) = handle.insert_source(ping_rx, move |_, _, state| {
        let new_theme = cosmic::theme::system_preference();
        let theme = &mut state.common.theme;

        if theme.theme_type != new_theme.theme_type {
            *theme = new_theme;
            let mut workspace_guard = state.common.workspace_state.update();
            state.common.shell.write().set_theme(
                theme.clone(),
                &state.common.xdg_activation_state,
                &mut workspace_guard,
            );
        }
    }) {
        tracing::error!("{e}");
    };

    let ping_tx_clone = ping_tx.clone();
    let theme_watcher_mode = config_mode_helper.watch(move |_, _keys| {
        ping_tx_clone.ping();
    })?;
    let ping_tx_clone = ping_tx.clone();
    let theme_watcher_light = config_light_helper.watch(move |_, _keys| {
        ping_tx_clone.ping();
    })?;
    let theme_watcher_dark = config_dark_helper.watch(move |_, _keys| {
        ping_tx.ping();
    })?;

    std::mem::forget(theme_watcher_dark);
    std::mem::forget(theme_watcher_light);
    std::mem::forget(theme_watcher_mode);

    // Initialize the global and also watch for live updates.
    set_lunaris_theme(lunaris_theme::LunarisTheme::load());

    let (lt_ping_tx, lt_ping_rx) = calloop::ping::make_ping().unwrap();
    if let Err(e) = handle.insert_source(lt_ping_rx, move |_, _, state| {
        let lt = lunaris_theme::LunarisTheme::load();
        set_lunaris_theme(lt.clone());
        state.common.lunaris_theme = lt.clone();
        let mut shell = state.common.shell.write();
        shell.lunaris_theme = lt;
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

    Ok(())
}
