// SPDX-License-Identifier: GPL-3.0-only

//! Handler implementation for the `lunaris-shell-overlay-v1` protocol.

use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;

use crate::{
    delegate_shell_overlay,
    shell::{SeatExt, grabs::menu::{Item, SeatMenuGrabState}},
    utils::prelude::*,
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
        tracing::info!("context_menu_activate: menu_id={menu_id} index={index}");
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
        tracing::info!("context_menu_activate: grab released for menu_id={menu_id}");
        // Notify desktop-shell that the menu is closed so it can restore
        // click-through on the layer surface (set_ignore_cursor_events(true)).
        self.common
            .shell_overlay_state
            .close_context_menu(menu_id);
        tracing::info!("context_menu_activate: context_menu_closed sent for menu_id={menu_id}");
    }

    fn context_menu_dismiss(&mut self, menu_id: u32) {
        tracing::info!("context_menu_dismiss: menu_id={menu_id}");
        self.common.pending_menu_callbacks.remove(&menu_id);
        unset_overlay_grab(self, menu_id);
        tracing::info!("context_menu_dismiss: grab released for menu_id={menu_id}");
        self.common
            .shell_overlay_state
            .close_context_menu(menu_id);
        tracing::info!("context_menu_dismiss: context_menu_closed sent for menu_id={menu_id}");
    }

    fn zoom_increase(&mut self) {
        let seat = self.common.shell.read().seats.last_active().clone();
        self.update_zoom(&seat, self.common.config.cosmic_conf.accessibility_zoom.increment as f64 / 100.0, true);
    }

    fn zoom_decrease(&mut self) {
        let seat = self.common.shell.read().seats.last_active().clone();
        self.update_zoom(&seat, -(self.common.config.cosmic_conf.accessibility_zoom.increment as f64 / 100.0), true);
    }

    fn zoom_close(&mut self) {
        self.common.config.cosmic_conf.accessibility_zoom.show_overlay = false;
        self.common.update_config();
    }

    fn zoom_set_increment(&mut self, value: u32) {
        self.common.config.cosmic_conf.accessibility_zoom.increment = value;
        self.common.update_config();
    }

    fn zoom_set_movement(&mut self, mode: u32) {
        use cosmic_comp_config::ZoomMovement;
        let movement = match mode {
            1 => ZoomMovement::Continuously,
            2 => ZoomMovement::OnEdge,
            3 => ZoomMovement::Centered,
            _ => return,
        };
        self.common.config.cosmic_conf.accessibility_zoom.view_moves = movement;
        self.common.update_config();
    }

    fn window_header_action(&mut self, surface_id: u32, action: u32) {
        use smithay::utils::SERIAL_COUNTER;

        let shell = self.common.shell.read();
        let mapped = shell.mapped().find(|m| {
            m.active_window()
                .wl_surface()
                .map(|s| {
                    use smithay::reexports::wayland_server::Resource;
                    let id: u32 = s.as_ref().id().protocol_id();
                    id == surface_id
                })
                .unwrap_or(false)
        });

        let Some(mapped) = mapped else {
            tracing::warn!(
                "shell_overlay: window_header_action for unknown surface_id {}",
                surface_id
            );
            return;
        };

        let surface = mapped.active_window();
        match action {
            1 => {
                // Minimize
                surface.set_minimized(true);
            }
            2 => {
                // Toggle maximize
                let maximized = surface.is_maximized(false);
                surface.set_maximized(!maximized);
                if let Some(toplevel) = surface.0.toplevel() {
                    toplevel.send_configure();
                }
            }
            3 => {
                // Close
                surface.close();
            }
            4 => {
                // Move -- start interactive move via the last active seat
                let seat = shell.seats.last_active().clone();
                let serial = SERIAL_COUNTER.next_serial();
                if let Some(wl_surface) = surface.wl_surface().map(std::borrow::Cow::into_owned) {
                    let evlh = self.common.event_loop_handle.clone();
                    std::mem::drop(shell);
                    let res = self.common.shell.write().move_request(
                        &wl_surface,
                        &seat,
                        serial,
                        crate::shell::grabs::ReleaseMode::NoMouseButtons,
                        false,
                        &self.common.config,
                        &evlh,
                        false,
                    );
                    if let Some((grab, focus)) = res {
                        if grab.is_touch_grab() {
                            seat.get_touch().unwrap().set_grab(self, grab, serial);
                        } else {
                            seat.get_pointer()
                                .unwrap()
                                .set_grab(self, grab, serial, focus);
                        }
                    }
                    return;
                }
            }
            _ => {
                tracing::warn!(
                    "shell_overlay: unknown window_header_action {} for surface {}",
                    action,
                    surface_id
                );
            }
        }
        std::mem::drop(shell);
    }

    fn set_layout_mode(&mut self, mode: u32) {
        use crate::shell::LayoutMode;

        let shell = self.common.shell.read();
        let seat = shell.seats.last_active().clone();
        let output = seat.active_output();
        drop(shell);

        let target_mode = match mode {
            0 => LayoutMode::Floating,
            1 => LayoutMode::Tiling,
            2 => LayoutMode::Monocle,
            _ => {
                tracing::warn!("shell_overlay: unknown layout mode {mode}");
                return;
            }
        };

        let mut shell = self.common.shell.write();
        let mut guard = self.common.workspace_state.update();

        let Some(workspace) = shell.workspaces.active_mut(&output) else {
            return;
        };

        let current = workspace.layout_mode;
        if current == target_mode {
            return;
        }

        tracing::info!("set_layout_mode: {:?} -> {:?}", current, target_mode);

        match target_mode {
            LayoutMode::Monocle => {
                if current == LayoutMode::Monocle {
                    return;
                }
                workspace.enter_monocle(&seat, &mut guard);
            }
            LayoutMode::Tiling => {
                if current == LayoutMode::Monocle {
                    workspace.exit_monocle(&seat, &mut guard);
                }
                if !workspace.tiling_enabled {
                    workspace.set_tiling(true, &seat, &mut guard);
                }
                workspace.layout_mode = LayoutMode::Tiling;
            }
            LayoutMode::Floating => {
                if current == LayoutMode::Monocle {
                    workspace.exit_monocle(&seat, &mut guard);
                }
                if workspace.tiling_enabled {
                    workspace.set_tiling(false, &seat, &mut guard);
                }
                workspace.layout_mode = LayoutMode::Floating;
            }
        }

        drop(shell);
        drop(guard);

        // Notify the shell of the change.
        self.common
            .shell_overlay_state
            .send_layout_mode_changed(target_mode as u32);
    }

    fn tab_activate(&mut self, stack_id: u32, index: u32) {
        let found = {
            let shell = self.common.shell.read();
            shell.mapped().find_map(|mapped| {
                let stack = mapped.stack_ref()?;
                if stack.stack_id() != stack_id {
                    return None;
                }
                let surface = stack.surfaces().nth(index as usize)?;
                Some((stack.clone(), surface))
            })
        };
        if let Some((stack, surface)) = found {
            stack.set_active(&surface);
        } else {
            tracing::warn!(
                "shell_overlay: tab_activate for unknown stack_id {}",
                stack_id
            );
        }
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
            // Pointer focus re-evaluation happens in the commit handler
            // when the layer surface input region is updated by desktop-shell.
        }
    }
}

delegate_shell_overlay!(State);
