// SPDX-License-Identifier: GPL-3.0-only

//! Server-side implementation of the `lunaris-window-attach-v1`
//! Wayland protocol. This protocol is ground-work for eliminating
//! the one-frame lag between a compositor-managed window and its
//! shell-rendered Lunaris header — see Feature 4 in `docs/`.
//!
//! v1 implements the **binding state** only: the shell can claim an
//! attachment object for one of its `wl_surface`s, bind it to an
//! opaque `window_id` (the same id carried by
//! `lunaris-shell-overlay::window_header_show`), and adjust the
//! offset/size over the attachment's lifetime. v1 does NOT yet
//! change how the compositor renders those surfaces — that's
//! phase 2, gated on subsurface-style atomic commit across two
//! clients. v1's value today is:
//!
//! 1. The compositor can prove at session startup whether the
//!    current shell is attach-aware.
//! 2. The lifecycle (`attach_to_window` / `unbound` / `destroy`)
//!    is well-defined and stable, so phase 2 is a purely internal
//!    renderer change.
//! 3. `ATTACH-DEBUG` logging on every request gives a clear audit
//!    trail during testing.

use std::collections::HashMap;

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::wl_surface::WlSurface,
};
use wayland_backend::server::GlobalId;

pub use generated::lunaris_window_attach_manager_v1;
pub use generated::lunaris_window_attachment_v1;
use generated::lunaris_window_attach_manager_v1::{
    LunarisWindowAttachManagerV1, Request as ManagerRequest,
};
use generated::lunaris_window_attachment_v1::{
    LunarisWindowAttachmentV1, Request as AttachmentRequest,
};

#[allow(non_snake_case, non_upper_case_globals, non_camel_case_types)]
mod generated {
    use smithay::reexports::wayland_server::{self, protocol::*};

    pub mod __interfaces {
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        use wayland_backend;
        wayland_scanner::generate_interfaces!(
            "resources/protocols/lunaris-window-attach-v1.xml"
        );
    }

    use self::__interfaces::*;

    wayland_scanner::generate_server_code!(
        "resources/protocols/lunaris-window-attach-v1.xml"
    );
}

// ===== Global data =====

/// Per-global data for the attach manager. Same `client_has_no_
/// security_context` filter pattern as the other Lunaris shell
/// protocols.
pub struct WindowAttachGlobalData {
    pub filter: Box<dyn for<'a> Fn(&'a Client) -> bool + Send + Sync>,
}

// ===== Per-attachment user data =====

/// Opaque window id (matches the `surface_id` in
/// `lunaris-shell-overlay::window_header_show`) and last-known
/// geometry an attachment declares. `None` on `window_id` means the
/// attachment was created but `attach_to_window` hasn't been called
/// yet.
#[derive(Debug, Clone, Default)]
pub struct AttachmentUserData {
    inner: std::sync::Arc<std::sync::Mutex<AttachmentInner>>,
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentInner {
    pub window_id: Option<u32>,
    pub offset_x: i32,
    pub offset_y: i32,
    pub width: i32,
    pub height: i32,
    pub surface: Option<WlSurface>,
}

impl AttachmentUserData {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, AttachmentInner> {
        self.inner.lock().unwrap()
    }
}

// ===== State =====

/// Server-side state for `lunaris-window-attach-v1`. Tracks live
/// manager globals and lets the compositor iterate bound
/// attachments grouped by window_id.
#[derive(Debug, Default)]
pub struct WindowAttachState {
    managers: Vec<LunarisWindowAttachManagerV1>,
    global: Option<GlobalId>,
    /// Reverse lookup: window_id → attachments bound to it. Useful
    /// for the phase-2 renderer and for emitting `unbound` when a
    /// window disappears.
    bindings: HashMap<u32, Vec<LunarisWindowAttachmentV1>>,
}

impl WindowAttachState {
    pub fn new<D, F>(dh: &DisplayHandle, client_filter: F) -> Self
    where
        D: GlobalDispatch<LunarisWindowAttachManagerV1, WindowAttachGlobalData>
            + Dispatch<LunarisWindowAttachManagerV1, ()>
            + Dispatch<LunarisWindowAttachmentV1, AttachmentUserData>
            + WindowAttachHandler
            + 'static,
        F: for<'a> Fn(&'a Client) -> bool + Send + Sync + 'static,
    {
        let global = dh.create_global::<D, LunarisWindowAttachManagerV1, _>(
            1,
            WindowAttachGlobalData {
                filter: Box::new(client_filter),
            },
        );
        tracing::info!("ATTACH-DEBUG WindowAttachState initialized, global registered");
        Self {
            managers: Vec::new(),
            global: Some(global),
            bindings: HashMap::new(),
        }
    }

    /// Emit `unbound` to every attachment currently bound to
    /// `window_id`, then drop them from the `bindings` index. The
    /// attachment objects themselves remain alive until the shell
    /// explicitly destroys them; see the `<event name="unbound">`
    /// contract in the XML.
    pub fn unbind_window(&mut self, window_id: u32) {
        if let Some(attachments) = self.bindings.remove(&window_id) {
            tracing::info!(
                "ATTACH-DEBUG unbind_window window_id={} n_attachments={}",
                window_id,
                attachments.len()
            );
            for attachment in &attachments {
                attachment.unbound();
                if let Some(ud) = attachment.data::<AttachmentUserData>() {
                    ud.lock().window_id = None;
                }
            }
        }
    }

    /// Returns a snapshot of (window_id, surface, offset, size) for
    /// every currently bound attachment. Intended for the phase-2
    /// renderer loop. Today this is still unused — the
    /// `ATTACH-DEBUG` log proves the state is being maintained.
    pub fn snapshot(&self) -> Vec<AttachmentSnapshot> {
        let mut out = Vec::new();
        for (window_id, atts) in &self.bindings {
            for att in atts {
                if let Some(ud) = att.data::<AttachmentUserData>() {
                    let inner = ud.lock();
                    if let Some(surface) = inner.surface.as_ref() {
                        out.push(AttachmentSnapshot {
                            window_id: *window_id,
                            surface: surface.clone(),
                            offset_x: inner.offset_x,
                            offset_y: inner.offset_y,
                            width: inner.width,
                            height: inner.height,
                        });
                    }
                }
            }
        }
        out
    }

    /// Returns `true` if at least one shell client has bound the
    /// attach manager global this session. Useful for choosing
    /// between legacy rendering and attach-aware rendering.
    pub fn has_attach_aware_shell(&self) -> bool {
        !self.managers.is_empty()
    }
}

/// Snapshot row used by phase-2 rendering (see `snapshot`).
#[derive(Debug, Clone)]
pub struct AttachmentSnapshot {
    pub window_id: u32,
    pub surface: WlSurface,
    pub offset_x: i32,
    pub offset_y: i32,
    pub width: i32,
    pub height: i32,
}

// ===== Handler trait =====

/// Hook trait for the attach protocol. Mostly a formality today
/// because v1 doesn't dispatch any interesting callbacks back to
/// the compositor's main state — it's here for consistency with
/// the other Lunaris protocols and to anchor phase-2 extension.
pub trait WindowAttachHandler {
    fn window_attach_state(&mut self) -> &mut WindowAttachState;
}

// ===== Global dispatch =====

impl<D> GlobalDispatch<LunarisWindowAttachManagerV1, WindowAttachGlobalData, D> for WindowAttachState
where
    D: GlobalDispatch<LunarisWindowAttachManagerV1, WindowAttachGlobalData>
        + Dispatch<LunarisWindowAttachManagerV1, ()>
        + Dispatch<LunarisWindowAttachmentV1, AttachmentUserData>
        + WindowAttachHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<LunarisWindowAttachManagerV1>,
        _global_data: &WindowAttachGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        tracing::info!(
            "ATTACH-DEBUG manager bound (client bound lunaris_window_attach_manager_v1)"
        );
        state.window_attach_state().managers.push(manager);
    }

    fn can_view(client: Client, global_data: &WindowAttachGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// ===== Manager dispatch =====

impl<D> Dispatch<LunarisWindowAttachManagerV1, (), D> for WindowAttachState
where
    D: Dispatch<LunarisWindowAttachManagerV1, ()>
        + Dispatch<LunarisWindowAttachmentV1, AttachmentUserData>
        + WindowAttachHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &LunarisWindowAttachManagerV1,
        request: ManagerRequest,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ManagerRequest::Destroy => {
                tracing::debug!("ATTACH-DEBUG manager::destroy");
            }
            ManagerRequest::GetAttachment { id, surface } => {
                let user_data = AttachmentUserData::default();
                {
                    let mut inner = user_data.lock();
                    inner.surface = Some(surface.clone());
                }
                let attachment = data_init.init(id, user_data);
                tracing::info!(
                    "ATTACH-DEBUG manager::get_attachment for surface {:?} (attachment created, still dormant)",
                    surface.id()
                );
                // The attachment starts dormant — no entry in
                // `bindings` until `attach_to_window` lands.
                let _ = attachment; // keep returned-resource warning quiet
                let _ = state;
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: wayland_backend::server::ClientId,
        resource: &LunarisWindowAttachManagerV1,
        _data: &(),
    ) {
        tracing::debug!("ATTACH-DEBUG manager destroyed");
        state
            .window_attach_state()
            .managers
            .retain(|m| m != resource);
    }
}

// ===== Attachment dispatch =====

impl<D> Dispatch<LunarisWindowAttachmentV1, AttachmentUserData, D> for WindowAttachState
where
    D: Dispatch<LunarisWindowAttachmentV1, AttachmentUserData>
        + WindowAttachHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &LunarisWindowAttachmentV1,
        request: AttachmentRequest,
        data: &AttachmentUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            AttachmentRequest::Destroy => {
                tracing::debug!("ATTACH-DEBUG attachment::destroy");
                // Remove from bindings if bound.
                let old_window_id = data.lock().window_id.take();
                if let Some(wid) = old_window_id {
                    let bindings = &mut state.window_attach_state().bindings;
                    if let Some(list) = bindings.get_mut(&wid) {
                        list.retain(|a| a != resource);
                        if list.is_empty() {
                            bindings.remove(&wid);
                        }
                    }
                }
            }
            AttachmentRequest::AttachToWindow {
                window_id,
                offset_x,
                offset_y,
                width,
                height,
            } => {
                tracing::info!(
                    "ATTACH-DEBUG attachment::attach_to_window window_id={} \
                     offset=({},{}) size={}x{}",
                    window_id, offset_x, offset_y, width, height
                );
                // Rebind: drop from previous window's list.
                let old_window_id = {
                    let mut inner = data.lock();
                    let prev = inner.window_id;
                    inner.window_id = Some(window_id);
                    inner.offset_x = offset_x;
                    inner.offset_y = offset_y;
                    inner.width = width;
                    inner.height = height;
                    prev
                };
                let bindings = &mut state.window_attach_state().bindings;
                if let Some(prev) = old_window_id {
                    if prev != window_id
                        && let Some(list) = bindings.get_mut(&prev)
                    {
                        list.retain(|a| a != resource);
                        if list.is_empty() {
                            bindings.remove(&prev);
                        }
                    }
                }
                bindings
                    .entry(window_id)
                    .or_default()
                    .push(resource.clone());
            }
            AttachmentRequest::UpdateGeometry {
                offset_x,
                offset_y,
                width,
                height,
            } => {
                tracing::debug!(
                    "ATTACH-DEBUG attachment::update_geometry offset=({},{}) size={}x{}",
                    offset_x, offset_y, width, height
                );
                let mut inner = data.lock();
                if inner.window_id.is_none() {
                    // Silently ignored per XML spec — attachment
                    // must be bound for geometry updates to mean
                    // anything.
                    return;
                }
                inner.offset_x = offset_x;
                inner.offset_y = offset_y;
                inner.width = width;
                inner.height = height;
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: wayland_backend::server::ClientId,
        resource: &LunarisWindowAttachmentV1,
        data: &AttachmentUserData,
    ) {
        tracing::debug!("ATTACH-DEBUG attachment object destroyed");
        let window_id = data.lock().window_id.take();
        if let Some(wid) = window_id {
            let bindings = &mut state.window_attach_state().bindings;
            if let Some(list) = bindings.get_mut(&wid) {
                list.retain(|a| a != resource);
                if list.is_empty() {
                    bindings.remove(&wid);
                }
            }
        }
    }
}

// ===== Delegate macro =====

/// Delegate `lunaris_window_attach_manager_v1` and
/// `lunaris_window_attachment_v1` dispatch to [`WindowAttachState`].
#[macro_export]
macro_rules! delegate_window_attach {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::wayland::protocols::window_attach::lunaris_window_attach_manager_v1::LunarisWindowAttachManagerV1:
                $crate::wayland::protocols::window_attach::WindowAttachGlobalData
        ] => $crate::wayland::protocols::window_attach::WindowAttachState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::window_attach::lunaris_window_attach_manager_v1::LunarisWindowAttachManagerV1: ()
        ] => $crate::wayland::protocols::window_attach::WindowAttachState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::wayland::protocols::window_attach::lunaris_window_attachment_v1::LunarisWindowAttachmentV1:
                $crate::wayland::protocols::window_attach::AttachmentUserData
        ] => $crate::wayland::protocols::window_attach::WindowAttachState);
    };
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_default_is_dormant() {
        let d = AttachmentUserData::default();
        let inner = d.lock();
        assert!(inner.window_id.is_none());
        assert_eq!(inner.offset_x, 0);
        assert_eq!(inner.offset_y, 0);
        assert_eq!(inner.width, 0);
        assert_eq!(inner.height, 0);
        assert!(inner.surface.is_none());
    }

    #[test]
    fn snapshot_is_empty_without_bindings() {
        let state = WindowAttachState::default();
        assert!(state.snapshot().is_empty());
        assert!(!state.has_attach_aware_shell());
    }
}
