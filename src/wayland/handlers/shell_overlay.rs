// SPDX-License-Identifier: GPL-3.0-only

//! Handler implementation for the `lunaris-shell-overlay-v1` protocol.

use calloop::LoopHandle;

use crate::{
    delegate_shell_overlay,
    shell::grabs::menu::Item,
    state::State,
    wayland::protocols::shell_overlay::{
        ContextMenuItem, ShellOverlayHandler, ShellOverlayState, WindowAction,
    },
};

impl ShellOverlayHandler for State {
    fn shell_overlay_state(&mut self) -> &mut ShellOverlayState {
        &mut self.common.shell_overlay_state
    }

    fn context_menu_activate(&mut self, menu_id: u32, index: u32) {
        let Some(callbacks) = self.common.pending_menu_callbacks.remove(&menu_id) else {
            tracing::warn!(
                "shell_overlay: received activate for unknown menu_id {}",
                menu_id
            );
            return;
        };

        let action_index = index as usize;
        if let Some(item) = callbacks.get(action_index) {
            if let Item::Entry { on_press, .. } = item {
                let on_press = on_press.clone();
                let _ = self
                    .common
                    .event_loop_handle
                    .insert_idle(move |state| (on_press)(&state.common.event_loop_handle));
            }
        } else {
            tracing::warn!(
                "shell_overlay: activate index {} out of range for menu_id {}",
                action_index,
                menu_id
            );
        }
    }

    fn context_menu_dismiss(&mut self, menu_id: u32) {
        self.common.pending_menu_callbacks.remove(&menu_id);
    }
}

delegate_shell_overlay!(State);
