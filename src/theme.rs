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

/// Active window hint color from LunarisTheme as [r, g, b].
pub(crate) fn lunaris_hint_rgb(lt: &lunaris_theme::LunarisTheme) -> [f32; 3] {
    if let Some(hint) = lt.window_hint {
        [hint[0], hint[1], hint[2]]
    } else {
        lt.accent_rgb()
    }
}

/// Initialize the global LunarisTheme and start a file watcher for live updates.
pub fn watch_theme(handle: LoopHandle<'_, State>) {
    // Initialize the global.
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
}
