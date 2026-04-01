// SPDX-License-Identifier: GPL-3.0-only

//! Server-side implementation of the `lunaris-shell-overlay-v1` Wayland protocol.
//!
//! This protocol allows the Lunaris compositor to delegate rendering of shell
//! overlay elements (context menus, tab bars, indicators) to the desktop-shell
//! process. The compositor sends overlay events; the desktop-shell sends back
//! user actions.

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_backend::server::GlobalId;

pub use generated::lunaris_shell_overlay_v1;
use generated::lunaris_shell_overlay_v1::{LunarisShellOverlayV1, Request as OverlayRequest};

#[allow(non_snake_case, non_upper_case_globals, non_camel_case_types)]
mod generated {
    use smithay::reexports::wayland_server::{self, protocol::*};

    pub mod __interfaces {
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        use wayland_backend;
        wayland_scanner::generate_interfaces!(
            "resources/protocols/lunaris-shell-overlay.xml"
        );
    }

    use self::__interfaces::*;

    wayland_scanner::generate_server_code!(
        "resources/protocols/lunaris-shell-overlay.xml"
    );
}

// ===== Global data =====

/// Per-global data for the `lunaris_shell_overlay_v1` global.
///
/// Holds the client filter that determines which clients may bind this global.
pub struct ShellOverlayGlobalData {
    /// Returns `true` if the given client is permitted to bind this global.
    pub filter: Box<dyn for<'a> Fn(&'a Client) -> bool + Send + Sync>,
}

// ===== State =====

/// Server-side state for the `lunaris-shell-overlay-v1` protocol.
///
/// Tracks all bound instances and assigns monotonically increasing menu IDs
/// to each context menu sequence.
#[derive(Debug)]
pub struct ShellOverlayState {
    instances: Vec<LunarisShellOverlayV1>,
    global: GlobalId,
    next_menu_id: u32,
}

impl ShellOverlayState {
    /// Create a new `ShellOverlayState` and register the global with the display.
    ///
    /// `client_filter` is called for each connecting client; return `true` to
    /// allow the client to bind the global.
    pub fn new<D, F>(dh: &DisplayHandle, client_filter: F) -> ShellOverlayState
    where
        D: GlobalDispatch<LunarisShellOverlayV1, ShellOverlayGlobalData>
            + Dispatch<LunarisShellOverlayV1, ()>
            + ShellOverlayHandler
            + 'static,
        F: for<'a> Fn(&'a Client) -> bool + Send + Sync + 'static,
    {
        let global = dh.create_global::<D, LunarisShellOverlayV1, _>(
            1,
            ShellOverlayGlobalData {
                filter: Box::new(client_filter),
            },
        );
        ShellOverlayState {
            instances: Vec::new(),
            global,
            next_menu_id: 0,
        }
    }

    /// Returns the `GlobalId` of the registered Wayland global.
    pub fn global_id(&self) -> GlobalId {
        self.global.clone()
    }

    /// Returns the first connected shell overlay instance, if any.
    ///
    /// Used by the compositor to identify the desktop-shell client for
    /// pointer focus routing during context menu grabs.
    pub fn overlay_instance(&self) -> Option<&LunarisShellOverlayV1> {
        self.instances.first()
    }

    /// Send a context menu sequence to all connected shell clients.
    ///
    /// Sends `context_menu_begin`, one event per item, then `context_menu_done`.
    ///
    /// Returns the `menu_id` assigned to this menu, or `None` if no shell
    /// client is currently connected.
    pub fn send_context_menu(
        &mut self,
        x: i32,
        y: i32,
        items: &[ContextMenuItem],
    ) -> Option<u32> {
        if self.instances.is_empty() {
            return None;
        }

        let menu_id = self.next_menu_id;
        self.next_menu_id = self.next_menu_id.wrapping_add(1);

        for instance in &self.instances {
            instance.context_menu_begin(menu_id, x, y);

            for (index, item) in items.iter().enumerate() {
                match item {
                    ContextMenuItem::Separator => {
                        instance.context_menu_separator(menu_id, index as u32);
                    }
                    ContextMenuItem::Entry {
                        action,
                        toggled,
                        disabled,
                        shortcut,
                    } => {
                        instance.context_menu_item(
                            menu_id,
                            index as u32,
                            lunaris_shell_overlay_v1::WindowAction::try_from(*action as u32)
                                .unwrap(),
                            *toggled as u32,
                            *disabled as u32,
                            shortcut.clone().unwrap_or_default(),
                        );
                    }
                }
            }

            instance.context_menu_done(menu_id);
        }

        Some(menu_id)
    }

    /// Notify all connected shell clients that a context menu was closed by the
    /// compositor (e.g. the associated window was closed or focus was lost).
    ///
    /// The shell must hide the menu without sending `activate` or `dismiss`.
    pub fn close_context_menu(&self, menu_id: u32) {
        for instance in &self.instances {
            instance.context_menu_closed(menu_id);
        }
    }
}

// ===== Tab bar methods =====

impl ShellOverlayState {
    /// Notify connected shells that a tab bar should be shown at the given position.
    pub fn send_tab_bar_show(&self, stack_id: u32, x: i32, y: i32, width: i32, height: i32) {
        for instance in &self.instances {
            instance.tab_bar_show(stack_id, x, y, width, height);
        }
    }

    /// Notify connected shells that a tab bar should be hidden.
    pub fn send_tab_bar_hide(&self, stack_id: u32) {
        for instance in &self.instances {
            instance.tab_bar_hide(stack_id);
        }
    }

    /// Notify connected shells that a tab was added to a stack.
    pub fn send_tab_added(
        &self,
        stack_id: u32,
        index: u32,
        title: String,
        app_id: String,
        active: bool,
    ) {
        for instance in &self.instances {
            instance.tab_added(stack_id, index, title.clone(), app_id.clone(), active as u32);
        }
    }

    /// Notify connected shells that a tab was removed from a stack.
    pub fn send_tab_removed(&self, stack_id: u32, index: u32) {
        for instance in &self.instances {
            instance.tab_removed(stack_id, index);
        }
    }

    /// Notify connected shells that the active tab changed.
    pub fn send_tab_activated(&self, stack_id: u32, index: u32) {
        for instance in &self.instances {
            instance.tab_activated(stack_id, index);
        }
    }

    /// Notify connected shells that a tab title changed.
    pub fn send_tab_title_changed(&self, stack_id: u32, index: u32, title: String) {
        for instance in &self.instances {
            instance.tab_title_changed(stack_id, index, title.clone());
        }
    }
}

// ===== Menu item types =====

/// A window management action that can appear as a context menu entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WindowAction {
    /// Minimize the window.
    Minimize = 1,
    /// Toggle maximize.
    Maximize = 2,
    /// Toggle fullscreen.
    Fullscreen = 3,
    /// Toggle tiled mode.
    Tiled = 4,
    /// Start an interactive move.
    Move = 5,
    /// Start a resize from the top edge.
    ResizeTop = 6,
    /// Start a resize from the left edge.
    ResizeLeft = 7,
    /// Start a resize from the right edge.
    ResizeRight = 8,
    /// Start a resize from the bottom edge.
    ResizeBottom = 9,
    /// Stack this window with another.
    Stack = 10,
    /// Remove this tab from its stack.
    Unstack = 11,
    /// Remove all tabs from a stack.
    UnstackAll = 12,
    /// Take a screenshot of the window.
    Screenshot = 13,
    /// Move the window to the previous workspace.
    MovePrevWorkspace = 14,
    /// Move the window to the next workspace.
    MoveNextWorkspace = 15,
    /// Toggle sticky (show on all workspaces).
    Sticky = 16,
    /// Close the window.
    Close = 17,
    /// Close all windows in a stack.
    CloseAll = 18,
}

impl TryFrom<u32> for WindowAction {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Minimize),
            2 => Ok(Self::Maximize),
            3 => Ok(Self::Fullscreen),
            4 => Ok(Self::Tiled),
            5 => Ok(Self::Move),
            6 => Ok(Self::ResizeTop),
            7 => Ok(Self::ResizeLeft),
            8 => Ok(Self::ResizeRight),
            9 => Ok(Self::ResizeBottom),
            10 => Ok(Self::Stack),
            11 => Ok(Self::Unstack),
            12 => Ok(Self::UnstackAll),
            13 => Ok(Self::Screenshot),
            14 => Ok(Self::MovePrevWorkspace),
            15 => Ok(Self::MoveNextWorkspace),
            16 => Ok(Self::Sticky),
            17 => Ok(Self::Close),
            18 => Ok(Self::CloseAll),
            _ => Err(()),
        }
    }
}

/// A single item in a context menu sent to the shell.
#[derive(Debug, Clone)]
pub enum ContextMenuItem {
    /// A visual separator between groups of items.
    Separator,
    /// An actionable entry.
    Entry {
        /// The window management action this item triggers.
        action: WindowAction,
        /// Whether the item is in a toggled/checked state (e.g. "Sticky" when active).
        toggled: bool,
        /// Whether the item is disabled and cannot be activated.
        disabled: bool,
        /// Optional keyboard shortcut label shown alongside the item.
        shortcut: Option<String>,
    },
}

// ===== Handler trait =====

/// Handler trait for `lunaris-shell-overlay-v1` compositor-side logic.
///
/// Implement this on your compositor state to receive shell overlay events.
pub trait ShellOverlayHandler {
    /// Returns a mutable reference to the `ShellOverlayState`.
    fn shell_overlay_state(&mut self) -> &mut ShellOverlayState;

    /// Called when the shell activates a context menu item.
    ///
    /// The compositor should look up the pending callbacks for `menu_id` and
    /// invoke the one at position `action`.
    fn context_menu_activate(&mut self, menu_id: u32, index: u32);

    /// Called when the shell dismisses a context menu without activating an item.
    fn context_menu_dismiss(&mut self, menu_id: u32);

    /// Called when the shell activates a tab in a stack.
    fn tab_activate(&mut self, stack_id: u32, index: u32);
}

// ===== GlobalDispatch =====

impl<D> GlobalDispatch<LunarisShellOverlayV1, ShellOverlayGlobalData, D>
    for ShellOverlayState
where
    D: GlobalDispatch<LunarisShellOverlayV1, ShellOverlayGlobalData>
        + Dispatch<LunarisShellOverlayV1, ()>
        + ShellOverlayHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<LunarisShellOverlayV1>,
        _global_data: &ShellOverlayGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let instance = data_init.init(resource, ());
        state.shell_overlay_state().instances.push(instance);
    }

    fn can_view(client: Client, global_data: &ShellOverlayGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// ===== Dispatch =====

impl<D> Dispatch<LunarisShellOverlayV1, (), D> for ShellOverlayState
where
    D: Dispatch<LunarisShellOverlayV1, ()> + ShellOverlayHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &LunarisShellOverlayV1,
        request: OverlayRequest,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            OverlayRequest::Activate { menu_id, index } => {
                state.context_menu_activate(menu_id, index);
            }
            OverlayRequest::Dismiss { menu_id } => {
                state.context_menu_dismiss(menu_id);
            }
            OverlayRequest::TabActivate { stack_id, index } => {
                state.tab_activate(stack_id, index);
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: wayland_backend::server::ClientId,
        resource: &LunarisShellOverlayV1,
        _data: &(),
    ) {
        state
            .shell_overlay_state()
            .instances
            .retain(|i| i != resource);
    }
}

// ===== Delegate macro =====

/// Delegate `lunaris_shell_overlay_v1` dispatch to [`ShellOverlayState`].
///
/// Call this macro once in the crate that implements the compositor state.
#[macro_export]
macro_rules! delegate_shell_overlay {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::wayland::protocols::shell_overlay::lunaris_shell_overlay_v1::LunarisShellOverlayV1:
                $crate::wayland::protocols::shell_overlay::ShellOverlayGlobalData
        ] => $crate::wayland::protocols::shell_overlay::ShellOverlayState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::shell_overlay::lunaris_shell_overlay_v1::LunarisShellOverlayV1: ()
        ] => $crate::wayland::protocols::shell_overlay::ShellOverlayState);
    };
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_action_roundtrip() {
        for value in 1u32..=18 {
            let action = WindowAction::try_from(value).expect("value in range should parse");
            assert_eq!(action as u32, value, "roundtrip failed for value {value}");
        }
    }

    #[test]
    fn window_action_invalid_returns_err() {
        assert!(WindowAction::try_from(0).is_err());
        assert!(WindowAction::try_from(19).is_err());
        assert!(WindowAction::try_from(u32::MAX).is_err());
    }

    #[test]
    fn menu_id_wraps_without_panic() {
        // Verify wrapping_add does not panic at u32::MAX
        let id = u32::MAX;
        let next = id.wrapping_add(1);
        assert_eq!(next, 0);
    }
}
