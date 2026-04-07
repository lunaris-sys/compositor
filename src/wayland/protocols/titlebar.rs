/// Server-side implementation of the `lunaris-titlebar-v1` Wayland protocol.
///
/// Apps declare titlebar content (tabs, buttons, breadcrumbs); the
/// compositor decides rendering based on window mode.
///
/// See `docs/architecture/titlebar-protocol.md`.

use std::collections::HashMap;

use smithay::reexports::wayland_server::{
    backend::GlobalId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

pub use generated::lunaris_titlebar_v1;
pub use generated::lunaris_titlebar_manager_v1;

// ---------------------------------------------------------------------------
// Scanner bindings
// ---------------------------------------------------------------------------

#[allow(non_snake_case, non_upper_case_globals, non_camel_case_types)]
mod generated {
    use smithay::reexports::wayland_server::{self, protocol::*};

    pub mod __interfaces {
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        use wayland_backend;
        wayland_scanner::generate_interfaces!(
            "resources/protocols/lunaris-titlebar-v1.xml"
        );
    }

    use self::__interfaces::*;

    wayland_scanner::generate_server_code!(
        "resources/protocols/lunaris-titlebar-v1.xml"
    );
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Titlebar mode as seen by the compositor.
///
/// Matches the `mode` enum in `lunaris-titlebar-v1.xml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum TitlebarMode {
    /// Normal floating window: show maximize + close.
    #[default]
    Floating = 0,
    /// Tiled window: show close only (maximize not applicable).
    Tiled = 1,
    /// Fullscreen: titlebar hidden (edge-reveal handled separately).
    Fullscreen = 2,
    /// No titlebar rendered at all.
    Frameless = 3,
}

/// Per-window titlebar state tracked by the compositor.
#[derive(Debug, Clone, Default)]
pub struct TitlebarState {
    pub title: String,
    pub tabs: Vec<TabInfo>,
    pub active_tab: Option<String>,
    pub buttons: Vec<ButtonInfo>,
    pub breadcrumb_json: String,
    pub center_content: u32,
    pub search_mode: bool,
    /// Current window mode (compositor-driven).
    pub mode: TitlebarMode,
}

/// Information about a single tab.
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub status: u32,
}

/// Information about a custom button.
#[derive(Debug, Clone)]
pub struct ButtonInfo {
    pub id: String,
    pub icon: String,
    pub tooltip: String,
    pub position: u32,
    pub enabled: bool,
}

/// Global state for the titlebar protocol.
#[derive(Debug)]
pub struct TitlebarManagerState {
    global: GlobalId,
    /// Per-surface titlebar state.
    pub surfaces: HashMap<u64, TitlebarState>,
    /// Per-surface protocol resources for sending events back to the client.
    resources: HashMap<u64, generated::lunaris_titlebar_v1::LunarisTitlebarV1>,
}

impl TitlebarManagerState {
    /// Register the global and return the state.
    pub fn new(display: &DisplayHandle) -> Self
    where
        crate::state::State: GlobalDispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>
            + Dispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>
            + Dispatch<generated::lunaris_titlebar_v1::LunarisTitlebarV1, u64>
            + 'static,
    {
        let global = display.create_global::<crate::state::State, generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>(1, ());
        Self {
            global,
            surfaces: HashMap::new(),
            resources: HashMap::new(),
        }
    }

    /// Get titlebar state for a surface (by surface id).
    pub fn get(&self, surface_id: u64) -> Option<&TitlebarState> {
        self.surfaces.get(&surface_id)
    }

    /// Get mutable titlebar state, creating default if missing.
    pub fn get_or_create(&mut self, surface_id: u64) -> &mut TitlebarState {
        self.surfaces.entry(surface_id).or_default()
    }

    /// Store the per-surface protocol resource for sending events.
    pub fn register_resource(
        &mut self,
        surface_id: u64,
        resource: generated::lunaris_titlebar_v1::LunarisTitlebarV1,
    ) {
        self.resources.insert(surface_id, resource);
    }

    /// Remove a per-surface resource (called on destroy).
    pub fn unregister_resource(&mut self, surface_id: u64) {
        self.resources.remove(&surface_id);
    }

    /// Send `mode_changed` event to the client for a surface.
    ///
    /// Returns `true` if the mode actually changed and the event was sent.
    pub fn send_mode_changed(&mut self, surface_id: u64, mode: TitlebarMode) -> bool {
        let Some(tb) = self.surfaces.get_mut(&surface_id) else {
            return false;
        };
        if tb.mode == mode {
            return false;
        }
        tb.mode = mode;
        if let Some(resource) = self.resources.get(&surface_id) {
            let proto_mode = match mode {
                TitlebarMode::Floating => generated::lunaris_titlebar_v1::Mode::Floating,
                TitlebarMode::Tiled => generated::lunaris_titlebar_v1::Mode::Tiled,
                TitlebarMode::Fullscreen => generated::lunaris_titlebar_v1::Mode::Fullscreen,
                TitlebarMode::Frameless => generated::lunaris_titlebar_v1::Mode::Frameless,
            };
            resource.mode_changed(proto_mode);
        }
        true
    }

    /// Check whether a surface has an active titlebar binding.
    pub fn has_titlebar(&self, surface_id: u64) -> bool {
        self.resources.contains_key(&surface_id)
    }

    /// Iterate over all surface IDs with active titlebar bindings.
    pub fn active_surface_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.resources.keys().copied()
    }
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// Trait that the compositor State must implement to handle titlebar requests.
pub trait TitlebarHandler {
    /// Access the titlebar manager state.
    fn titlebar_manager_state(&mut self) -> &mut TitlebarManagerState;

    /// Called after every state-mutating titlebar request.
    ///
    /// The implementation should serialize the titlebar state for the given
    /// surface and forward it to the shell via the overlay protocol's
    /// `window_header_content` event.
    fn notify_titlebar_changed(&mut self, surface_id: u64);

    /// Called when a titlebar object is destroyed.
    ///
    /// The implementation should notify the shell that titlebar content
    /// for this surface is no longer available.
    fn notify_titlebar_removed(&mut self, surface_id: u64);
}

// ---------------------------------------------------------------------------
// GlobalDispatch: lunaris_titlebar_manager_v1
// ---------------------------------------------------------------------------

impl<D> GlobalDispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, (), D>
    for TitlebarManagerState
where
    D: GlobalDispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>
        + Dispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>
        + Dispatch<generated::lunaris_titlebar_v1::LunarisTitlebarV1, u64>
        + TitlebarHandler
        + 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

// ---------------------------------------------------------------------------
// Dispatch: lunaris_titlebar_manager_v1 (factory requests)
// ---------------------------------------------------------------------------

impl<D> Dispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, (), D>
    for TitlebarManagerState
where
    D: Dispatch<generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1, ()>
        + Dispatch<generated::lunaris_titlebar_v1::LunarisTitlebarV1, u64>
        + TitlebarHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &generated::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1,
        request: generated::lunaris_titlebar_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            generated::lunaris_titlebar_manager_v1::Request::GetTitlebar { id, surface } => {
                // Use wl_surface ID as our surface key.
                let surface_id = surface.id().protocol_id() as u64;
                let mgr = state.titlebar_manager_state();
                // Ensure state entry exists.
                mgr.get_or_create(surface_id);
                let resource = data_init.init(id, surface_id);
                state.titlebar_manager_state().register_resource(surface_id, resource);
            }
            generated::lunaris_titlebar_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch: lunaris_titlebar_v1 (per-surface requests)
// ---------------------------------------------------------------------------

impl<D> Dispatch<generated::lunaris_titlebar_v1::LunarisTitlebarV1, u64, D>
    for TitlebarManagerState
where
    D: Dispatch<generated::lunaris_titlebar_v1::LunarisTitlebarV1, u64>
        + TitlebarHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &generated::lunaris_titlebar_v1::LunarisTitlebarV1,
        request: generated::lunaris_titlebar_v1::Request,
        surface_id: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use crate::wayland::handlers::titlebar::*;

        let sid = *surface_id;
        let mut changed = true;

        {
            let tb = state.titlebar_manager_state().get_or_create(sid);

            match request {
                generated::lunaris_titlebar_v1::Request::SetTitle { title } => {
                    tb.title = title;
                }
                generated::lunaris_titlebar_v1::Request::SetBreadcrumb { segments_json } => {
                    tb.breadcrumb_json = segments_json;
                }
                generated::lunaris_titlebar_v1::Request::SetCenterContent { content } => {
                    tb.center_content = content.into();
                }
                generated::lunaris_titlebar_v1::Request::AddTab {
                    id,
                    title,
                    icon,
                    status,
                } => {
                    handle_add_tab(tb, &id, &title, icon.as_deref(), status.into());
                }
                generated::lunaris_titlebar_v1::Request::RemoveTab { id } => {
                    handle_remove_tab(tb, &id);
                }
                generated::lunaris_titlebar_v1::Request::UpdateTab { id, title, status } => {
                    handle_update_tab(tb, &id, &title, status.into());
                }
                generated::lunaris_titlebar_v1::Request::ActivateTab { id } => {
                    handle_activate_tab(tb, &id);
                }
                generated::lunaris_titlebar_v1::Request::ReorderTabs { ids_json } => {
                    handle_reorder_tabs(tb, &ids_json);
                }
                generated::lunaris_titlebar_v1::Request::AddButton {
                    id,
                    icon,
                    tooltip,
                    position,
                } => {
                    handle_add_button(tb, &id, &icon, &tooltip, position.into());
                }
                generated::lunaris_titlebar_v1::Request::RemoveButton { id } => {
                    handle_remove_button(tb, &id);
                }
                generated::lunaris_titlebar_v1::Request::SetButtonEnabled { id, enabled } => {
                    handle_set_button_enabled(tb, &id, enabled != 0);
                }
                generated::lunaris_titlebar_v1::Request::SetSearchMode { enabled } => {
                    tb.search_mode = enabled != 0;
                }
                generated::lunaris_titlebar_v1::Request::Destroy => {
                    changed = false;
                }
                _ => {
                    changed = false;
                }
            }
        }

        // Borrow on titlebar_manager_state is dropped; safe to access
        // shell_overlay_state now.
        if changed {
            state.notify_titlebar_changed(sid);
        }
    }

    fn destroyed(
        state: &mut D,
        _client: wayland_backend::server::ClientId,
        _resource: &generated::lunaris_titlebar_v1::LunarisTitlebarV1,
        surface_id: &u64,
    ) {
        let sid = *surface_id;
        let mgr = state.titlebar_manager_state();
        mgr.surfaces.remove(&sid);
        mgr.unregister_resource(sid);
        state.notify_titlebar_removed(sid);
    }
}

// ---------------------------------------------------------------------------
// Delegate macro
// ---------------------------------------------------------------------------

/// Delegates `GlobalDispatch` and `Dispatch` for the titlebar protocol to
/// `TitlebarManagerState`.
#[macro_export]
macro_rules! delegate_titlebar {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::wayland::protocols::titlebar::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1: ()
        ] => $crate::wayland::protocols::titlebar::TitlebarManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::titlebar::lunaris_titlebar_manager_v1::LunarisTitlebarManagerV1: ()
        ] => $crate::wayland::protocols::titlebar::TitlebarManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::titlebar::lunaris_titlebar_v1::LunarisTitlebarV1: u64
        ] => $crate::wayland::protocols::titlebar::TitlebarManagerState);
    };
}
