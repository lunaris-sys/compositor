// SPDX-License-Identifier: GPL-3.0-only
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

pub struct ShellOverlayGlobalData {
    pub filter: Box<dyn for<'a> Fn(&'a Client) -> bool + Send + Sync>,
}

// ===== State =====

#[derive(Debug)]
pub struct ShellOverlayState {
    instances: Vec<LunarisShellOverlayV1>,
    global: GlobalId,
    next_menu_id: u32,
}

impl ShellOverlayState {
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

    pub fn global_id(&self) -> GlobalId {
        self.global.clone()
    }

    /// Send a context menu to all connected shell clients.
    /// Returns the menu_id assigned to this menu, or None if no client is connected.
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
                            lunaris_shell_overlay_v1::WindowAction::try_from(*action as u32).unwrap(),
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

    /// Notify shell that a menu was closed by the compositor (e.g. window closed).
    pub fn close_context_menu(&self, menu_id: u32) {
        for instance in &self.instances {
            instance.context_menu_closed(menu_id);
        }
    }
}

// ===== Menu item types =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WindowAction {
    Minimize = 1,
    Maximize = 2,
    Fullscreen = 3,
    Tiled = 4,
    Move = 5,
    ResizeTop = 6,
    ResizeLeft = 7,
    ResizeRight = 8,
    ResizeBottom = 9,
    Stack = 10,
    Unstack = 11,
    UnstackAll = 12,
    Screenshot = 13,
    MovePrevWorkspace = 14,
    MoveNextWorkspace = 15,
    Sticky = 16,
    Close = 17,
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

#[derive(Debug, Clone)]
pub enum ContextMenuItem {
    Separator,
    Entry {
        action: WindowAction,
        toggled: bool,
        disabled: bool,
        shortcut: Option<String>,
    },
}

// ===== Handler trait =====

pub trait ShellOverlayHandler {
    fn shell_overlay_state(&mut self) -> &mut ShellOverlayState;

    /// Called when the shell activates a context menu item.
    fn context_menu_activate(&mut self, menu_id: u32, action: WindowAction);

    /// Called when the shell dismisses a context menu without activating an item.
    fn context_menu_dismiss(&mut self, menu_id: u32);
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
                if let Ok(action) = WindowAction::try_from(index) {
                    state.context_menu_activate(menu_id, action);
                } else {
                    tracing::warn!(
                        "shell_overlay: received activate with unknown action index {}",
                        index
                    );
                }
            }
            OverlayRequest::Dismiss { menu_id } => {
                state.context_menu_dismiss(menu_id);
            }
            _ => {}
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
