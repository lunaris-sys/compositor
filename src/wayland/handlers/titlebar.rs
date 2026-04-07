/// Handler for the `lunaris-titlebar-v1` Wayland protocol.
///
/// Receives titlebar content declarations from apps and updates the
/// per-surface TitlebarState. The actual rendering is delegated to the
/// desktop shell via the `lunaris-shell-overlay` protocol's
/// `window_header_*` events.
///
/// See `docs/architecture/titlebar-protocol.md`.

use crate::{
    delegate_titlebar,
    state::{Common, State},
    wayland::protocols::titlebar::{
        ButtonInfo, TabInfo, TitlebarHandler, TitlebarManagerState, TitlebarMode, TitlebarState,
    },
};

impl TitlebarHandler for State {
    fn titlebar_manager_state(&mut self) -> &mut TitlebarManagerState {
        &mut self.common.titlebar_manager_state
    }

    fn notify_titlebar_changed(&mut self, surface_id: u64) {
        let json = if let Some(tb) = self.common.titlebar_manager_state.get(surface_id) {
            state_to_json(tb)
        } else {
            return;
        };
        self.common
            .shell_overlay_state
            .send_window_header_content(surface_id as u32, json);
    }

    fn notify_titlebar_removed(&mut self, surface_id: u64) {
        // Send empty JSON object so the shell knows to clear titlebar content.
        self.common
            .shell_overlay_state
            .send_window_header_content(surface_id as u32, "{}".to_string());
    }
}

delegate_titlebar!(State);

/// Update the titlebar mode for a surface based on current window state.
///
/// Called from shell transition points (tiling, fullscreen, etc.) where
/// `Common` is accessible. Sends `mode_changed` to the client app and
/// `window_header_content` to the shell if the mode changed.
///
/// `surface_id` is the `wl_surface` protocol ID.
/// `is_tiled`, `is_fullscreen` reflect the current window state.
pub fn update_titlebar_mode(
    common: &mut Common,
    surface_id: u64,
    is_tiled: bool,
    is_fullscreen: bool,
) {
    if !common.titlebar_manager_state.has_titlebar(surface_id) {
        return;
    }

    let mode = if is_fullscreen {
        TitlebarMode::Fullscreen
    } else if is_tiled {
        TitlebarMode::Tiled
    } else {
        TitlebarMode::Floating
    };

    if !common.titlebar_manager_state.send_mode_changed(surface_id, mode) {
        return; // Mode unchanged.
    }

    // Mode changed; also update the shell overlay with new content JSON.
    if let Some(tb) = common.titlebar_manager_state.get(surface_id) {
        let json = state_to_json(tb);
        common
            .shell_overlay_state
            .send_window_header_content(surface_id as u32, json);
    }
}

/// Process a tab addition.
pub fn handle_add_tab(state: &mut TitlebarState, id: &str, title: &str, icon: Option<&str>, status: u32) {
    // Remove existing tab with same ID (update case).
    state.tabs.retain(|t| t.id != id);
    state.tabs.push(TabInfo {
        id: id.to_string(),
        title: title.to_string(),
        icon: icon.map(String::from),
        status,
    });
}

/// Process a tab removal.
pub fn handle_remove_tab(state: &mut TitlebarState, id: &str) {
    state.tabs.retain(|t| t.id != id);
    if state.active_tab.as_deref() == Some(id) {
        state.active_tab = state.tabs.first().map(|t| t.id.clone());
    }
}

/// Process a tab update.
pub fn handle_update_tab(state: &mut TitlebarState, id: &str, title: &str, status: u32) {
    if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) {
        tab.title = title.to_string();
        tab.status = status;
    }
}

/// Process tab activation.
pub fn handle_activate_tab(state: &mut TitlebarState, id: &str) {
    state.active_tab = Some(id.to_string());
}

/// Process tab reorder.
pub fn handle_reorder_tabs(state: &mut TitlebarState, ids_json: &str) {
    if let Ok(ids) = serde_json::from_str::<Vec<String>>(ids_json) {
        let mut reordered = Vec::new();
        for id in &ids {
            if let Some(tab) = state.tabs.iter().find(|t| &t.id == id) {
                reordered.push(tab.clone());
            }
        }
        // Append any tabs not in the reorder list (shouldn't happen but safety).
        for tab in &state.tabs {
            if !ids.contains(&tab.id) {
                reordered.push(tab.clone());
            }
        }
        state.tabs = reordered;
    }
}

/// Process button addition.
pub fn handle_add_button(
    state: &mut TitlebarState,
    id: &str,
    icon: &str,
    tooltip: &str,
    position: u32,
) {
    state.buttons.retain(|b| b.id != id);
    state.buttons.push(ButtonInfo {
        id: id.to_string(),
        icon: icon.to_string(),
        tooltip: tooltip.to_string(),
        position,
        enabled: true,
    });
}

/// Process button removal.
pub fn handle_remove_button(state: &mut TitlebarState, id: &str) {
    state.buttons.retain(|b| b.id != id);
}

/// Process button enable/disable.
pub fn handle_set_button_enabled(state: &mut TitlebarState, id: &str, enabled: bool) {
    if let Some(btn) = state.buttons.iter_mut().find(|b| b.id == id) {
        btn.enabled = enabled;
    }
}

/// Serialize the titlebar mode as a string for JSON.
fn mode_str(mode: TitlebarMode) -> &'static str {
    match mode {
        TitlebarMode::Floating => "floating",
        TitlebarMode::Tiled => "tiled",
        TitlebarMode::Fullscreen => "fullscreen",
        TitlebarMode::Frameless => "frameless",
    }
}

pub fn state_to_json(state: &TitlebarState) -> String {
    serde_json::json!({
        "mode": mode_str(state.mode),
        "title": state.title,
        "tabs": state.tabs.iter().map(|t| serde_json::json!({
            "id": t.id,
            "title": t.title,
            "icon": t.icon,
            "status": t.status,
        })).collect::<Vec<_>>(),
        "active_tab": state.active_tab,
        "buttons": state.buttons.iter().map(|b| serde_json::json!({
            "id": b.id,
            "icon": b.icon,
            "tooltip": b.tooltip,
            "position": b.position,
            "enabled": b.enabled,
        })).collect::<Vec<_>>(),
        "breadcrumb": state.breadcrumb_json,
        "center_content": state.center_content,
        "search_mode": state.search_mode,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_state() -> TitlebarState {
        TitlebarState::default()
    }

    #[test]
    fn test_add_tab() {
        let mut state = empty_state();
        handle_add_tab(&mut state, "tab1", "Hello", None, 0);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].title, "Hello");
    }

    #[test]
    fn test_remove_tab() {
        let mut state = empty_state();
        handle_add_tab(&mut state, "tab1", "A", None, 0);
        handle_add_tab(&mut state, "tab2", "B", None, 0);
        handle_remove_tab(&mut state, "tab1");
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, "tab2");
    }

    #[test]
    fn test_remove_active_tab_fallback() {
        let mut state = empty_state();
        handle_add_tab(&mut state, "tab1", "A", None, 0);
        handle_add_tab(&mut state, "tab2", "B", None, 0);
        handle_activate_tab(&mut state, "tab1");
        handle_remove_tab(&mut state, "tab1");
        assert_eq!(state.active_tab.as_deref(), Some("tab2"));
    }

    #[test]
    fn test_update_tab() {
        let mut state = empty_state();
        handle_add_tab(&mut state, "tab1", "Old", None, 0);
        handle_update_tab(&mut state, "tab1", "New", 1);
        assert_eq!(state.tabs[0].title, "New");
        assert_eq!(state.tabs[0].status, 1);
    }

    #[test]
    fn test_reorder_tabs() {
        let mut state = empty_state();
        handle_add_tab(&mut state, "a", "A", None, 0);
        handle_add_tab(&mut state, "b", "B", None, 0);
        handle_add_tab(&mut state, "c", "C", None, 0);
        handle_reorder_tabs(&mut state, r#"["c","a","b"]"#);
        assert_eq!(state.tabs[0].id, "c");
        assert_eq!(state.tabs[1].id, "a");
        assert_eq!(state.tabs[2].id, "b");
    }

    #[test]
    fn test_add_button() {
        let mut state = empty_state();
        handle_add_button(&mut state, "btn1", "save", "Save", 1);
        assert_eq!(state.buttons.len(), 1);
        assert_eq!(state.buttons[0].icon, "save");
        assert!(state.buttons[0].enabled);
    }

    #[test]
    fn test_set_button_enabled() {
        let mut state = empty_state();
        handle_add_button(&mut state, "btn1", "save", "Save", 1);
        handle_set_button_enabled(&mut state, "btn1", false);
        assert!(!state.buttons[0].enabled);
    }

    #[test]
    fn test_state_to_json() {
        let mut state = empty_state();
        state.title = "Test".into();
        handle_add_tab(&mut state, "t1", "Tab 1", Some("file"), 0);
        let json = state_to_json(&state);
        assert!(json.contains("Test"));
        assert!(json.contains("Tab 1"));
        assert!(json.contains("\"mode\":\"floating\""));
    }

    #[test]
    fn test_state_to_json_tiled_mode() {
        let mut state = empty_state();
        state.mode = TitlebarMode::Tiled;
        state.title = "Tiled Window".into();
        let json = state_to_json(&state);
        assert!(json.contains("\"mode\":\"tiled\""));
    }

    #[test]
    fn test_state_to_json_fullscreen_mode() {
        let mut state = empty_state();
        state.mode = TitlebarMode::Fullscreen;
        let json = state_to_json(&state);
        assert!(json.contains("\"mode\":\"fullscreen\""));
    }

    #[test]
    fn test_state_to_json_frameless_mode() {
        let mut state = empty_state();
        state.mode = TitlebarMode::Frameless;
        let json = state_to_json(&state);
        assert!(json.contains("\"mode\":\"frameless\""));
    }
}
