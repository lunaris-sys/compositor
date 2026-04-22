// SPDX-License-Identifier: GPL-3.0-only

//! Handler implementation for the `lunaris-window-attach-v1`
//! protocol. v1 has no interesting main-state dispatch — all
//! request handling lives in the protocol module — but the
//! `WindowAttachHandler` impl on `State` is the contract that ties
//! the two together and anchors the delegate macro.

use crate::{
    delegate_window_attach,
    state::State,
    wayland::protocols::window_attach::{WindowAttachHandler, WindowAttachState},
};

impl WindowAttachHandler for State {
    fn window_attach_state(&mut self) -> &mut WindowAttachState {
        &mut self.common.window_attach_state
    }
}

delegate_window_attach!(State);
