use std::{cell::RefCell, sync::Mutex};

use smithay::{
    delegate_kde_decoration, delegate_xdg_decoration,
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as XdgMode,
        wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
            Mode as KdeMode, OrgKdeKwinServerDecoration,
        },
        wayland_server::protocol::wl_surface::WlSurface,
    },
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
        shell::{
            kde::decoration::{KdeDecorationHandler, KdeDecorationState},
            xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
        },
    },
};
use wayland_backend::protocol::WEnum;

use crate::state::State;

pub struct PreferredDecorationMode(RefCell<Option<XdgMode>>);

impl PreferredDecorationMode {
    pub fn is_unset(window: &Window) -> bool {
        window
            .user_data()
            .get::<PreferredDecorationMode>()
            .is_none()
    }

    pub fn mode(window: &Window) -> Option<XdgMode> {
        let user_data = window.user_data();
        user_data.insert_if_missing(|| PreferredDecorationMode(RefCell::new(None)));
        *user_data
            .get::<PreferredDecorationMode>()
            .unwrap()
            .0
            .borrow()
    }

    pub fn update(window: &Window, update: Option<XdgMode>) {
        let user_data = window.user_data();
        user_data.insert_if_missing(|| PreferredDecorationMode(RefCell::new(None)));
        *user_data
            .get::<PreferredDecorationMode>()
            .unwrap()
            .0
            .borrow_mut() = update;
    }
}

pub type KdeDecorationData = Mutex<KdeDecorationSurfaceState>;
#[derive(Debug, Default)]
pub struct KdeDecorationSurfaceState {
    pub mode: Option<KdeMode>,
    pub objs: Vec<OrgKdeKwinServerDecoration>,
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // DECO-DEBUG: Log every xdg-decoration object creation.
        tracing::info!(
            "DECO-DEBUG xdg::new_decoration surface={:?}",
            toplevel.wl_surface()
        );

        // Defensive default: do NOT force a mode here.
        //
        // Previously the compositor hard-coded `ClientSide` in both
        // branches of the `is_stack()` if/else. That forced every
        // client to CSD even when it had just bound the xdg-
        // decoration protocol to ask for SSD. Kitty's observed
        // behaviour (logs confirmed) was `new_decoration` →
        // `request_mode(ServerSide)` almost immediately after —
        // so the old code's ClientSide write was overwritten 0.1ms
        // later. Clients that don't explicitly `set_mode` got stuck
        // with ClientSide even when they would have preferred SSD.
        //
        // Neutral default + trust the client's request_mode call:
        // GTK/Qt stay unaffected (they don't bind the protocol, so
        // Smithay's `is_decorated` defaults to true → client draws
        // CSD as before). Terminals (Kitty/Foot/Alacritty) explicitly
        // call `set_mode(ServerSide)` → they get SSD → Lunaris-
        // rendered header appears for them. Option B as specified.
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: XdgMode) {
        tracing::info!(
            "DECO-DEBUG xdg::request_mode surface={:?} requested_mode={:?}",
            toplevel.wl_surface(), mode
        );

        let shell = self.common.shell.read();
        if let Some(mapped) = shell.element_for_surface(toplevel.wl_surface()) {
            if let Some((window, _)) = mapped
                .windows()
                .find(|(window, _)| window.wl_surface().as_deref() == Some(toplevel.wl_surface()))
                && let Some(toplevel) = window.0.toplevel()
            {
                PreferredDecorationMode::update(&window.0, Some(mode));
                toplevel.with_pending_state(|state| {
                    state.decoration_mode = Some(mode);
                });
                toplevel.send_configure();
            }
        } else {
            toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        tracing::info!(
            "DECO-DEBUG xdg::unset_mode surface={:?}",
            toplevel.wl_surface()
        );

        let shell = self.common.shell.read();
        if let Some(mapped) = shell.element_for_surface(toplevel.wl_surface())
            && let Some((window, _)) = mapped
                .windows()
                .find(|(window, _)| window.wl_surface().as_deref() == Some(toplevel.wl_surface()))
            && let Some(toplevel) = window.0.toplevel()
        {
            PreferredDecorationMode::update(&window.0, None);
            toplevel.with_pending_state(|state| {
                state.decoration_mode = None;
            });
            toplevel.send_configure();
        }
    }
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.common.kde_decoration_state
    }

    fn new_decoration(&mut self, surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration) {
        tracing::info!(
            "DECO-DEBUG kde::new_decoration surface={:?}",
            surface
        );

        // Symmetric to the xdg-decoration path: neutral default.
        // We still stash the decoration object so `request_mode`
        // can later configure its state, but we don't advertise
        // any mode until the client picks one. Most KDE clients
        // (e.g. Qt 5/6) request Client mode explicitly on startup;
        // Kitty-likes that also bind kde-decoration request Server.
        with_states(surface, |states| {
            let mut state = states
                .data_map
                .get_or_insert_threadsafe::<KdeDecorationData, _>(Default::default)
                .lock()
                .unwrap();
            state.objs.push(decoration.clone());
        });
    }

    fn request_mode(
        &mut self,
        surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        mode: WEnum<KdeMode>,
    ) {
        tracing::info!(
            "DECO-DEBUG kde::request_mode surface={:?} mode={:?}",
            surface, mode
        );
        if let WEnum::Value(mode) = mode {
            with_states(surface, |states| {
                states
                    .data_map
                    .get_or_insert_threadsafe::<KdeDecorationData, _>(Default::default)
                    .lock()
                    .unwrap()
                    .mode = Some(mode);
            });
            decoration.mode(mode);
        }
    }

    fn release(&mut self, decoration: &OrgKdeKwinServerDecoration, surface: &WlSurface) {
        with_states(surface, |states| {
            let mut state = states
                .data_map
                .get_or_insert_threadsafe::<KdeDecorationData, _>(Default::default)
                .lock()
                .unwrap();

            state.objs.retain(|obj| obj != decoration);
            if state.objs.is_empty() {
                state.mode.take();
            }
        });
    }
}

delegate_xdg_decoration!(State);
delegate_kde_decoration!(State);
