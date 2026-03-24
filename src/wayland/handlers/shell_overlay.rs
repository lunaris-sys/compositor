// SPDX-License-Identifier: GPL-3.0-only

//! Handler implementation for the `lunaris-shell-overlay-v1` protocol.

use smithay::utils::SERIAL_COUNTER;

use crate::{
    delegate_shell_overlay,
    shell::grabs::menu::{Item, SeatMenuGrabState},
    state::State,
    wayland::protocols::shell_overlay::{
        ShellOverlayHandler, ShellOverlayState,
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

        unset_overlay_grab(self, menu_id);
    }

    fn context_menu_dismiss(&mut self, menu_id: u32) {
        self.common.pending_menu_callbacks.remove(&menu_id);
        unset_overlay_grab(self, menu_id);
    }
}

/// Release the pointer grab held by the `MenuGrab` that owns `menu_id`.
///
/// Iterates all seats, finds the one whose `SeatMenuGrabState` carries the
/// matching `menu_id`, and calls `unset_grab` on its pointer.
fn unset_overlay_grab(state: &mut State, menu_id: u32) {
    let matching_seat = {
        let shell = state.common.shell.read();
        shell
            .seats
            .iter()
            .find(|seat| {
                seat.user_data()
                    .get::<SeatMenuGrabState>()
                    .and_then(|g| {
                        g.lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|s| s.menu_id)
                    })
                    == Some(menu_id)
            })
            .cloned()
    };

    if let Some(seat) = matching_seat {
        if let Some(ptr) = seat.get_pointer() {
            ptr.unset_grab(state, SERIAL_COUNTER.next_serial(), 0);
        }
    }
}

delegate_shell_overlay!(State);
