use crate::{
    backend::render::{
        IndicatorShader, Key, Usage,
        clipped_surface::ClippedSurfaceRenderElement,
        cursor::CursorState,
        element::{AsGlowRenderer, FromGlesError},
        shadow::ShadowShader,
    },
    shell::{
        element::{CosmicMappedKey, CosmicMappedKeyInner},
        focus::target::PointerFocusTarget,
        grabs::{ReleaseMode, ResizeEdge},
    },
    state::State,
    utils::prelude::*,
};
use calloop::LoopHandle;
use cosmic_comp_config::AppearanceConfig;
use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            ImportAll, ImportMem, Renderer,
            element::{
                Element, Id as RendererId, Kind, RenderElement,
                UnderlyingStorage, memory::MemoryRenderBufferRenderElement,
                surface::WaylandSurfaceRenderElement,
            },
            gles::element::PixelShaderElement,
            glow::GlowRenderer,
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    desktop::{WindowSurfaceType, space::SpaceElement},
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
        },
        touch::{
            DownEvent, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchTarget,
            UpEvent,
        },
    },
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Buffer, IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size, Transform},
    wayland::seat::WaylandFocus,
};
use std::{
    borrow::Cow,
    fmt,
    hash::Hash,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};
use wayland_backend::server::ObjectId;

use super::CosmicSurface;

/// Height of server-side decoration header in logical pixels.
pub const SSD_HEIGHT: i32 = 36;
/// Width of the invisible resize border around windows.
pub const RESIZE_BORDER: i32 = 10;

/// A single window managed by the compositor, with optional server-side decorations.
#[derive(Clone)]
pub struct CosmicWindow {
    pub(super) inner: Arc<Mutex<CosmicWindowInternal>>,
    handle: LoopHandle<'static, crate::state::State>,
}

// SAFETY: LoopHandle contains an Rc internally, making CosmicWindow !Send/!Sync by
// default. All LoopHandle usage is on the main thread (calloop event loop), and the
// interior state is serialised through the Mutex. This mirrors CosmicStack.
unsafe impl Send for CosmicWindow {}
unsafe impl Sync for CosmicWindow {}

impl PartialEq for CosmicWindow {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CosmicWindow {}

impl Hash for CosmicWindow {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.inner).hash(state);
    }
}

impl fmt::Debug for CosmicWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CosmicWindow")
            .field("inner", &Arc::as_ptr(&self.inner))
            .finish_non_exhaustive()
    }
}

/// Internal state for a single managed window.
#[derive(Debug)]
pub struct CosmicWindowInternal {
    pub(super) window: CosmicSurface,
    activated: AtomicBool,
    /// TODO: This needs to be per seat
    pointer_entered: AtomicU8,
    last_title: Mutex<String>,
    tiled: AtomicBool,
    appearance_conf: Mutex<AppearanceConfig>,
    /// Compositor-rendered Lunaris header cache (Feature 4-C).
    /// Holds the last-rasterised `MemoryRenderBuffer` paired with
    /// the `HeaderVisualState` snapshot that produced it. On every
    /// frame, `CosmicWindow::header_render_element` builds a fresh
    /// `HeaderVisualState` from current inputs; if it equals the
    /// cached snapshot we reuse the existing buffer — no tiny-skia
    /// / cosmic-text work — otherwise we rasterise anew and
    /// replace. Keeps hot-path drag rendering at GPU-blit speed.
    pub(crate) header_cache: Mutex<
        Option<(
            crate::backend::render::window_header::HeaderVisualState,
            smithay::backend::renderer::element::memory::MemoryRenderBuffer,
        )>,
    >,
    /// Current pointer interaction with the window-control buttons,
    /// updated by `PointerTarget::motion` / `PointerTarget::button`
    /// when `current_focus() == Some(Focus::Header)`. Read by the
    /// rasteriser via `HeaderVisualState::interaction`.
    pub(crate) header_button_interaction:
        Mutex<crate::backend::render::window_header::ButtonInteraction>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    Header = 1,
    ResizeTop,
    ResizeLeft,
    ResizeRight,
    ResizeBottom,
    ResizeTopRight,
    ResizeTopLeft,
    ResizeBottomRight,
    ResizeBottomLeft,
}

impl Focus {
    pub fn under(
        surface: &CosmicSurface,
        header_height: i32,
        location: Point<f64, Logical>,
    ) -> Option<Focus> {
        // Corner tolerance: diagonal resize triggers when the pointer is
        // within this many pixels of a corner along both axes.
        const CORNER: i32 = 8;

        let geo = surface.geometry();
        let loc = location.to_i32_round::<i32>() - geo.loc;
        let bottom = header_height + geo.size.h;

        // Corners first (checked with tolerance zone).
        if loc.y < CORNER && loc.x < CORNER {
            Some(Focus::ResizeTopLeft)
        } else if loc.y < CORNER && loc.x >= geo.size.w - CORNER {
            Some(Focus::ResizeTopRight)
        } else if loc.y >= bottom - CORNER && loc.x < CORNER {
            Some(Focus::ResizeBottomLeft)
        } else if loc.y >= bottom - CORNER && loc.x >= geo.size.w - CORNER {
            Some(Focus::ResizeBottomRight)
        // Edges (only outside the window geometry, no tolerance overlap).
        } else if loc.y < 0 {
            Some(Focus::ResizeTop)
        } else if loc.y >= bottom {
            Some(Focus::ResizeBottom)
        } else if loc.x < 0 {
            Some(Focus::ResizeLeft)
        } else if loc.x >= geo.size.w {
            Some(Focus::ResizeRight)
        } else if loc.y < header_height {
            Some(Focus::Header)
        } else {
            None
        }
    }

    pub fn cursor_shape(&self) -> CursorIcon {
        match self {
            Focus::ResizeTopLeft => CursorIcon::NwResize,
            Focus::ResizeTopRight => CursorIcon::NeResize,
            Focus::ResizeTop => CursorIcon::NResize,
            Focus::ResizeBottomLeft => CursorIcon::SwResize,
            Focus::ResizeBottomRight => CursorIcon::SeResize,
            Focus::ResizeBottom => CursorIcon::SResize,
            Focus::ResizeLeft => CursorIcon::WResize,
            Focus::ResizeRight => CursorIcon::EResize,
            Focus::Header => CursorIcon::Default,
        }
    }

    pub unsafe fn from_u8(value: u8) -> Option<Focus> {
        match value {
            0 => None,
            focus => unsafe { Some(std::mem::transmute::<u8, Focus>(focus)) },
        }
    }
}

impl CosmicWindowInternal {
    /// Atomically swap the current focus state, returning the previous value.
    pub fn swap_focus(&self, focus: Option<Focus>) -> Option<Focus> {
        let value = focus.map_or(0, |x| x as u8);
        unsafe { Focus::from_u8(self.pointer_entered.swap(value, Ordering::SeqCst)) }
    }

    /// Returns the current focus state.
    pub fn current_focus(&self) -> Option<Focus> {
        unsafe { Focus::from_u8(self.pointer_entered.load(Ordering::SeqCst)) }
    }

    /// Returns if the window has any current or pending server-side decorations.
    pub fn has_ssd(&self, pending: bool) -> bool {
        !self.window.is_decorated(pending)
    }

    /// Returns if the window is currently tiled.
    pub fn is_tiled(&self) -> bool {
        self.tiled.load(Ordering::Acquire)
    }

    fn has_tiled_state(&self) -> bool {
        self.window.is_tiled(false).unwrap_or(false)
    }
}

impl CosmicWindow {
    /// Lock the internal state and return a guard.
    fn p(&self) -> MutexGuard<'_, CosmicWindowInternal> {
        self.inner.lock().unwrap()
    }

    /// Create a new CosmicWindow wrapping the given surface.
    pub fn new(
        window: impl Into<CosmicSurface>,
        handle: LoopHandle<'static, crate::state::State>,
        appearance: AppearanceConfig,
    ) -> CosmicWindow {
        let window = window.into();

        if appearance.clip_floating_windows {
            window.set_tiled(true);
        }

        CosmicWindow {
            inner: Arc::new(Mutex::new(CosmicWindowInternal {
                window,
                activated: AtomicBool::new(false),
                pointer_entered: AtomicU8::new(0),
                last_title: Mutex::new(String::new()),
                tiled: AtomicBool::new(false),
                appearance_conf: Mutex::new(appearance),
                header_cache: Mutex::new(None),
                header_button_interaction: Mutex::new(
                    crate::backend::render::window_header::ButtonInteraction::Idle,
                ),
            })),
            handle,
        }
    }

    /// Returns the pending size of the window including SSD header if applicable.
    pub fn pending_size(&self) -> Option<Size<i32, Logical>> {
        let p = self.p();
        let mut size = p.window.pending_size()?;
        if p.has_ssd(true) {
            size.h += SSD_HEIGHT;
        }
        Some(size)
    }

    /// Set the geometry of the window, accounting for SSD header offset.
    pub fn set_geometry(&self, geo: Rectangle<i32, Global>) {
        let p = self.p();
        let ssd_height = if p.has_ssd(true) { SSD_HEIGHT } else { 0 };
        let loc = (geo.loc.x, geo.loc.y + ssd_height);
        let size = (geo.size.w, std::cmp::max(geo.size.h - ssd_height, 0));
        p.window
            .set_geometry(Rectangle::new(loc.into(), size.into()), ssd_height as u32);
    }

    /// Handle a surface commit for this window.
    pub fn on_commit(&self, surface: &WlSurface) {
        let p = self.p();
        if &p.window == surface {
            p.window.0.on_commit();
        }
    }

    /// Return the underlying CosmicSurface.
    pub fn surface(&self) -> CosmicSurface {
        self.p().window.clone()
    }

    /// Find the focus target under a relative position.
    pub fn focus_under(
        &self,
        mut relative_pos: Point<f64, Logical>,
        surface_type: WindowSurfaceType,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        let p = self.p();
        let mut offset = Point::from((0., 0.));
        let mut window_ui = None;
        let has_ssd = p.has_ssd(false);
        if (has_ssd || p.has_tiled_state())
            && surface_type.contains(WindowSurfaceType::TOPLEVEL)
        {
            let geo = p.window.geometry();

            let point_i32 = relative_pos.to_i32_round::<i32>();
            let ssd_height = if has_ssd { SSD_HEIGHT } else { 0 };

            if (point_i32.x - geo.loc.x >= -RESIZE_BORDER && point_i32.x - geo.loc.x < 0)
                || (point_i32.y - geo.loc.y >= -RESIZE_BORDER && point_i32.y - geo.loc.y < 0)
                || (point_i32.x - geo.loc.x >= geo.size.w
                    && point_i32.x - geo.loc.x < geo.size.w + RESIZE_BORDER)
                || (point_i32.y - geo.loc.y >= geo.size.h + ssd_height
                    && point_i32.y - geo.loc.y < geo.size.h + ssd_height + RESIZE_BORDER)
            {
                window_ui = Some((
                    PointerFocusTarget::WindowUI(self.clone()),
                    Point::from((0., 0.)),
                ));
            }

            if has_ssd && (point_i32.y - geo.loc.y < SSD_HEIGHT) {
                window_ui = Some((
                    PointerFocusTarget::WindowUI(self.clone()),
                    Point::from((0., 0.)),
                ));
            }
        }

        if has_ssd {
            relative_pos.y -= SSD_HEIGHT as f64;
            offset.y += SSD_HEIGHT as f64;
        }

        window_ui.or_else(|| {
            p.window
                .focus_under(relative_pos, surface_type)
                .map(|(target, surface_offset)| (target, offset + surface_offset))
        })
    }

    /// Check whether this window wraps the given surface.
    pub fn contains_surface(&self, window: &CosmicSurface) -> bool {
        &self.p().window == window
    }

    /// Returns the offset of the client area relative to the window origin (SSD header).
    pub fn offset(&self) -> Point<i32, Logical> {
        let has_ssd = self.p().has_ssd(false);
        if has_ssd {
            Point::from((0, SSD_HEIGHT))
        } else {
            Point::from((0, 0))
        }
    }

    /// Returns a clone of the loop handle.
    pub(super) fn loop_handle(&self) -> LoopHandle<'static, crate::state::State> {
        self.handle.clone()
    }

    /// Render popup elements for this window.
    pub fn popup_render_elements<R, C>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C>
    where
        R: Renderer + ImportAll + ImportMem,
        R::TextureId: Send + Clone + 'static,
        C: From<CosmicWindowRenderElement<R>>,
    {
        let has_ssd = self.p().has_ssd(false);

        let window_loc = if has_ssd {
            location + Point::from((0, (SSD_HEIGHT as f64 * scale.y) as i32))
        } else {
            location
        };

        let p = self.p();
        p.window
            .popup_render_elements::<R, CosmicWindowRenderElement<R>>(
                renderer, window_loc, scale, alpha,
            )
            .into_iter()
            .map(C::from)
            .collect()
    }

    /// Render a shadow element for this window.
    pub fn shadow_render_element<R, C>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        max_size: Option<Size<i32, Logical>>,
        output_scale: Scale<f64>,
        scale: f64,
        alpha: f32,
    ) -> Option<C>
    where
        R: AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        C: From<CosmicWindowRenderElement<R>>,
    {
        let p = self.p();
        let has_ssd = p.has_ssd(false);
        let is_tiled = p.is_tiled();
        let activated = p.window.is_activated(false);
        let appearance = p.appearance_conf.lock().unwrap();

        if p.window.is_maximized(false) {
            return None;
        }

        let clip = (!is_tiled && appearance.clip_floating_windows)
            || (is_tiled && appearance.clip_tiled_windows);
        let should_draw_shadow = if is_tiled {
            appearance.shadow_tiled_windows
        } else {
            appearance.clip_floating_windows || has_ssd
        };

        if !should_draw_shadow {
            return None;
        }
        let lt = crate::theme::lunaris_theme();
        let mut radii = lt
            .radius_s
            .map(|x| if x < 4.0 { x } else { x + 4.0 })
            .map(|x| (x * scale as f32).round() as u8);
        if has_ssd && !clip {
            // bottom corners
            radii[0] = 0;
            radii[2] = 0;
            if is_tiled {
                // top corners
                radii[1] = 0;
                radii[3] = 0;
            }
        }

        let mut geo = SpaceElement::geometry(&p.window).to_f64();
        if has_ssd {
            geo.size.h += SSD_HEIGHT as f64;
        }
        geo = geo.upscale(scale);
        geo.loc += location.to_f64().to_logical(output_scale);
        if let Some(max_size) = max_size {
            geo.size = geo.size.clamp(Size::default(), max_size.to_f64());
        }

        let _appearance = appearance;
        drop(_appearance);

        let window_key =
            CosmicMappedKey(CosmicMappedKeyInner::Window(Arc::downgrade(&self.inner)));

        Some(
            CosmicWindowRenderElement::Shadow(ShadowShader::element(
                renderer,
                window_key,
                geo.to_i32_round().as_local(),
                radii,
                if activated { alpha } else { alpha * 0.75 },
                output_scale.x,
                lt.is_dark,
            ))
            .into(),
        )
    }

    /// Render all elements for this window (border, surface, clipping).
    pub fn render_elements<R, C>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        max_size: Option<Size<i32, Logical>>,
        scale: Scale<f64>,
        alpha: f32,
        scanout_override: Option<bool>,
    ) -> Vec<C>
    where
        R: AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        C: From<CosmicWindowRenderElement<R>>,
    {
        let (has_ssd, is_tiled, is_maximized, mut radii, appearance) = {
            let p = self.p();
            let raw_radius = crate::theme::lunaris_theme().radius_s;
            let mapped = raw_radius
                .map(|x| if x < 4.0 { x } else { x + 4.0 })
                .map(|x| x.round() as u8);
            // Trace: uncomment to debug radius propagation
            // tracing::trace!("window render: raw_radius={raw_radius:?} mapped={mapped:?}");
            (
                p.has_ssd(false),
                p.is_tiled(),
                p.window.is_maximized(false),
                mapped,
                *p.appearance_conf.lock().unwrap(),
            )
        };
        let clip = ((!is_tiled && appearance.clip_floating_windows)
            || (is_tiled && appearance.clip_tiled_windows))
            && !is_maximized;
        if has_ssd && !clip {
            // bottom corners
            radii[0] = 0;
            radii[2] = 0;
            if is_tiled {
                // top corners
                radii[1] = 0;
                radii[3] = 0;
            }
        }

        let window_loc = if has_ssd {
            location + Point::from((0, (SSD_HEIGHT as f64 * scale.y) as i32))
        } else {
            location
        };

        let mut elements = Vec::new();

        let mut geo = {
            let p = self.p();
            SpaceElement::geometry(&p.window).to_f64()
        };
        geo.loc += location.to_f64().to_logical(scale);
        if has_ssd {
            geo.size.h += SSD_HEIGHT as f64;
        }
        if let Some(max_size) = max_size {
            geo.size = geo.size.clamp(Size::default(), max_size.to_f64());
        }

        // Window border removed: desktop-shell handles all window chrome
        // via the shell overlay protocol. The previous 1px border used
        // lt.border (#e2e2e8) which appeared white on dark backgrounds.

        let window_elements = {
            let p = self.p();
            p.window
                .render_elements::<R, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    window_loc,
                    scale,
                    alpha,
                    scanout_override,
                )
        };
        if window_elements.is_empty() {
            return Vec::new();
        }

        elements.extend(window_elements.into_iter().map(|elem| {
            if has_ssd {
                radii[1] = 0;
                radii[3] = 0;
            }
            if radii.iter().any(|x| *x != 0)
                && clip
                && ClippedSurfaceRenderElement::will_clip(&elem, scale, geo, radii)
            {
                CosmicWindowRenderElement::Clipped(ClippedSurfaceRenderElement::new(
                    renderer, elem, scale, geo, radii,
                ))
            } else {
                CosmicWindowRenderElement::Window(elem)
            }
        }));

        // SSD header rendering removed: desktop-shell renders headers via protocol.

        // Feature 4-C: prepend the compositor-rasterised Lunaris
        // window header so it paints ON TOP of the client content.
        // `elements[0]` is topmost in smithay's render order, so we
        // insert at position 0. The header is eligible only for
        // SSD-mode windows (the compositor reserved SSD_HEIGHT px
        // at the top of the geometry for us); `header_render_element`
        // short-circuits to `None` otherwise.
        if let Some(header_element) =
            self.header_render_element::<R>(renderer, location, scale)
        {
            elements.insert(
                0,
                CosmicWindowRenderElement::from(
                    header_element,
                )
                .into(),
            );
        }

        elements.into_iter().map(C::from).collect()
    }

    /// Update the appearance configuration, adjusting tiling state if needed.
    pub fn update_appearance_conf(&self, appearance: &AppearanceConfig) {
        let p = self.p();
        let mut conf = p.appearance_conf.lock().unwrap();
        if &*conf != appearance {
            *conf = *appearance;
            if appearance.clip_floating_windows {
                p.window.set_tiled(true);
            } else if !p.tiled.load(Ordering::Acquire) {
                p.window.set_tiled(false);
            }
            p.window.send_configure();
        }
    }

    /// Force a redraw (no-op without Iced).
    pub(crate) fn force_redraw(&self) {
        // No-op: Iced rendering removed.
    }

    /// Return the header button (if any) that the given
    /// logical-space pointer position sits over. Logical space is
    /// the same one `PointerTarget::motion` / `button` receive via
    /// `event.location`. `None` when the pointer is in the drag
    /// zone or outside the header entirely.
    pub fn header_button_at(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<crate::backend::render::window_header::HeaderButton> {
        use crate::backend::render::window_header::{ButtonVisibility, HeaderVisualState, layout_buttons};
        let width = {
            let p = self.p();
            if !p.has_ssd(false) {
                return None;
            }
            p.window.geometry().size.w
        };
        let geo = {
            let p = self.p();
            p.window.geometry()
        };
        // Pointer position relative to the header's top-left. The
        // header sits at the window geometry's origin; for the
        // button layout we want the local (0,0) to be that origin.
        let local_x = (location.x - geo.loc.x as f64) as f32;
        let local_y = (location.y - geo.loc.y as f64) as f32;
        if local_y < 0.0 || local_y >= SSD_HEIGHT as f32 {
            return None;
        }
        let state = HeaderVisualState {
            width,
            activated: false,
            title: String::new(),
            buttons: ButtonVisibility { has_minimize: true, has_maximize: true },
            interaction: crate::backend::render::window_header::ButtonInteraction::Idle,
            scale: 1.0,
            focused_button: None,
            theme_generation: 0,
        };
        for b in layout_buttons(&state) {
            if b.hit_test(local_x, local_y) {
                return Some(b.button);
            }
        }
        None
    }

    /// Refresh the cached `ButtonInteraction` from the current
    /// pointer location. Called on every motion inside the header
    /// zone; the next render frame picks up the new state and
    /// either hits the pixmap cache (if the same button stays
    /// hovered) or re-rasterises with the new hover visual.
    ///
    /// Press-tracking is handled separately in `button` — this
    /// function only touches the Idle ↔ Hover transition, never
    /// stomps a Pressed state.
    pub(crate) fn update_header_button_interaction(
        &self,
        location: Point<f64, Logical>,
    ) {
        use crate::backend::render::window_header::ButtonInteraction;
        let current_focus = self.p().current_focus();
        if !matches!(current_focus, Some(Focus::Header)) {
            // Leaving the header zone settles on Idle unless a press
            // is still in flight.
            let p = self.p();
            let mut guard = p.header_button_interaction.lock().unwrap();
            if !matches!(*guard, ButtonInteraction::Pressed(_)) {
                *guard = ButtonInteraction::Idle;
            }
            return;
        }
        let over = self.header_button_at(location);
        let p = self.p();
        let mut guard = p.header_button_interaction.lock().unwrap();
        let prev = *guard;
        match *guard {
            ButtonInteraction::Pressed(pressed_btn) => {
                // Stay Pressed as long as the pointer remains over
                // the same button. If it drifts off the button, keep
                // the Pressed state to allow "press and drag off to
                // cancel" behaviour on release.
                if over != Some(pressed_btn) {
                    // No change — keep Pressed; release handler decides.
                    return;
                }
            }
            _ => {
                *guard = match over {
                    Some(b) => ButtonInteraction::Hover(b),
                    None => ButtonInteraction::Idle,
                };
            }
        }
        if *guard != prev {
            tracing::debug!(
                "HEADER-BTN-DEBUG hover transition: {:?} -> {:?}",
                prev, *guard
            );
        }
    }

    /// If the pointer is currently hovering a header button (as
    /// tracked by the most recent motion event),
    /// flip the interaction state to `Pressed` and return the
    /// button so the caller can intercept the left-press before
    /// `move_request` takes over. Returns `None` when the pointer
    /// is in the drag zone — caller falls back to the drag path.
    ///
    /// Reads ONLY the cached interaction state — no pointer
    /// location lookup — because `PointerTarget::button` runs
    /// under the pointer Mutex and `current_location()` would
    /// deadlock (see DoubleClickTracker comment for the full story).
    pub(crate) fn arm_header_button_press_from_hover(
        &self,
    ) -> Option<crate::backend::render::window_header::HeaderButton> {
        use crate::backend::render::window_header::ButtonInteraction;
        let p = self.p();
        let mut guard = p.header_button_interaction.lock().unwrap();
        match *guard {
            ButtonInteraction::Hover(b) => {
                *guard = ButtonInteraction::Pressed(b);
                Some(b)
            }
            ButtonInteraction::Pressed(b) => Some(b),
            ButtonInteraction::Idle => None,
        }
    }

    /// Handle a release in the header zone: if we were Pressed on
    /// a button and the pointer is still over the same button
    /// (inferred from the stored Hover/Pressed target matching),
    /// return it so the caller can invoke the action. Settles the
    /// interaction state back to Hover/Idle afterwards.
    pub(crate) fn finalize_header_button_release(
        &self,
    ) -> Option<crate::backend::render::window_header::HeaderButton> {
        use crate::backend::render::window_header::ButtonInteraction;
        let p = self.p();
        let mut guard = p.header_button_interaction.lock().unwrap();
        let fired = match *guard {
            // Pressed state was held until release, and motion only
            // clears it if the pointer left the button. So any
            // remaining `Pressed(b)` on release means the click
            // completed on the same button.
            ButtonInteraction::Pressed(b) => Some(b),
            _ => None,
        };
        // Settle back — stay Hover if the caller can tell us so, but
        // since we don't read location here, default to Idle. The
        // next motion event will re-arm Hover if appropriate.
        *guard = ButtonInteraction::Idle;
        fired
    }

    /// Produce a single `MemoryRenderBufferRenderElement` containing
    /// the compositor-rasterised Lunaris header (Feature 4-C) at
    /// `physical_location`. Returns `None` when this window is not
    /// eligible for a header (CSD / stacked / fullscreen — see
    /// `crate::shell::should_render_window_header` for the full
    /// policy, but the authoritative eligibility check lives there
    /// to keep the policy in one place).
    ///
    /// The element is produced from a cached `MemoryRenderBuffer`
    /// keyed by a `HeaderVisualState`. Mouse motion that doesn't
    /// change the hovered button, focus toggles that don't change
    /// activated state, and re-renders with identical theme all
    /// hit the cache and skip tiny-skia completely.
    ///
    /// `physical_location` is the output-space top-left of the
    /// window including the 36-px header strip (i.e. the location
    /// the caller passes to `render_elements`).
    pub fn header_render_element<R>(
        &self,
        _renderer: &mut R,
        physical_location: Point<i32, Physical>,
        output_scale: Scale<f64>,
    ) -> Option<
        smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<R>,
    >
    where
        R: AsGlowRenderer + smithay::backend::renderer::ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        use crate::backend::render::window_header::{
            self as wh, ButtonVisibility, HeaderVisualState, rasterize_header,
        };

        let (title, activated, width_logical, interaction, buttons) = {
            let p = self.p();
            // Only windows with SSD decorations get a Lunaris header
            // (the `Shell::should_render_window_header` policy is
            // already applied by the caller for the WAYLAND path;
            // for X11 callers will gate via the same helper).
            // We additionally guard locally so a late-stage state
            // flip doesn't produce a zero-width header.
            if !p.has_ssd(false) {
                return None;
            }
            let title = p.window.title();
            let activated = p.window.is_activated(false);
            let geometry = p.window.geometry();
            let width = geometry.size.w;
            let interaction = *p.header_button_interaction.lock().unwrap();
            // Minimize / maximize are always offered today. A
            // future refinement can gate these on
            // xdg_toplevel.wm_capabilities but the shell already
            // treats them as always-on so stay consistent.
            let buttons = ButtonVisibility { has_minimize: true, has_maximize: true };
            (title, activated, width, interaction, buttons)
        };
        if width_logical <= 0 {
            return None;
        }

        let state = HeaderVisualState {
            width: width_logical,
            activated,
            title,
            buttons,
            interaction,
            scale: output_scale.x,
            focused_button: None,
            theme_generation: wh::theme_generation(),
        };

        let buffer = {
            let p = self.p();
            let mut cache = p.header_cache.lock().unwrap();
            let needs_rerasterise = match cache.as_ref() {
                None => true,
                Some((cached_state, _)) => cached_state != &state,
            };
            if needs_rerasterise {
                let theme = crate::theme::lunaris_theme();
                let new_buffer = rasterize_header(&state, &theme);
                *cache = Some((state.clone(), new_buffer));
            }
            cache.as_ref().unwrap().1.clone()
        };

        // Convert physical location to logical coordinates
        // expected by `MemoryRenderBufferRenderElement::from_buffer`.
        // The header sits at the window's top-left corner.
        let logical_loc = physical_location.to_f64().to_logical(output_scale);
        let element =
            smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                _renderer,
                (logical_loc.x, logical_loc.y),
                &buffer,
                None,
                None,
                None,
                smithay::backend::renderer::element::Kind::Unspecified,
            )
            .ok();
        element
    }

    /// Returns the minimum size of the window including SSD header.
    pub fn min_size(&self) -> Option<Size<i32, Logical>> {
        let p = self.p();
        p.window.min_size_without_ssd().map(|size| {
            if p.has_ssd(false) {
                size + (0, SSD_HEIGHT).into()
            } else {
                size
            }
        })
    }

    /// Returns the maximum size of the window including SSD header.
    pub fn max_size(&self) -> Option<Size<i32, Logical>> {
        let p = self.p();
        p.window.max_size_without_ssd().map(|size| {
            if p.has_ssd(false) {
                size + (0, SSD_HEIGHT).into()
            } else {
                size
            }
        })
    }

    /// Set the tiled state of this window.
    pub fn set_tiled(&self, tiled: bool) {
        let p = self.p();
        p.tiled.store(tiled, Ordering::Release);
        if !p.appearance_conf.lock().unwrap().clip_floating_windows {
            p.window.set_tiled(tiled);
        }
    }

    /// Compute the corner radii for this window given geometry and defaults.
    pub fn corner_radius(&self, geometry_size: Size<i32, Logical>, default_radius: u8) -> [u8; 4] {
        let p = self.p();
        let has_ssd = p.has_ssd(false);
        let is_tiled = p.is_tiled();
        let appearance = p.appearance_conf.lock().unwrap();

        let clip = ((!is_tiled && appearance.clip_floating_windows)
            || (is_tiled && appearance.clip_tiled_windows))
            && !p.window.is_maximized(false);
        let round =
            (!is_tiled || appearance.clip_tiled_windows) && !p.window.is_maximized(false);
        let radii = if round {
            {
                crate::theme::lunaris_theme()
                    .radius_s
                    .map(|x| if x < 4.0 { x } else { x + 4.0 })
                    .map(|x| x.round() as u8)
            }
        } else {
            [0; 4]
        };

        match (has_ssd, clip) {
            (has_ssd, true) => {
                let mut corners = p.window.corner_radius(geometry_size).unwrap_or(radii);

                corners[0] = radii[0].max(corners[0]);
                corners[1] = if has_ssd {
                    radii[1]
                } else {
                    radii[1].max(corners[1])
                };
                corners[2] = radii[2].max(corners[2]);
                corners[3] = if has_ssd {
                    radii[3]
                } else {
                    radii[3].max(corners[3])
                };

                corners
            }
            (true, false) => p
                .window
                .corner_radius(geometry_size)
                .map(|[a, _, c, _]| [a, radii[1], c, radii[3]])
                .unwrap_or([default_radius, radii[1], default_radius, radii[3]]),
            (false, false) => p
                .window
                .corner_radius(geometry_size)
                .unwrap_or([default_radius; 4]),
        }
    }
}

impl IsAlive for CosmicWindow {
    fn alive(&self) -> bool {
        self.p().window.alive()
    }
}

impl SpaceElement for CosmicWindow {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        let p = self.p();
        let mut bbox = SpaceElement::bbox(&p.window);
        let has_ssd = p.has_ssd(false);

        if has_ssd || p.has_tiled_state() {
            bbox.loc -= Point::from((RESIZE_BORDER, RESIZE_BORDER));
            bbox.size += Size::from((RESIZE_BORDER * 2, RESIZE_BORDER * 2));
        }
        if has_ssd {
            bbox.size.h += SSD_HEIGHT;
        }

        bbox
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        self.focus_under(*point, WindowSurfaceType::ALL).is_some()
    }
    fn set_activate(&self, activated: bool) {
        let p = self.p();
        if p.activated.load(Ordering::SeqCst) != activated {
            p.activated.store(activated, Ordering::SeqCst);
            SpaceElement::set_activate(&p.window, activated);
        }
    }
    #[profiling::function]
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        let p = self.p();
        SpaceElement::output_enter(&p.window, output, overlap);
    }
    #[profiling::function]
    fn output_leave(&self, output: &Output) {
        let p = self.p();
        SpaceElement::output_leave(&p.window, output);
    }
    fn geometry(&self) -> Rectangle<i32, Logical> {
        let p = self.p();
        let mut geo = SpaceElement::geometry(&p.window);
        if p.has_ssd(false) {
            geo.size.h += SSD_HEIGHT;
        }
        geo
    }
    fn z_index(&self) -> u8 {
        let p = self.p();
        SpaceElement::z_index(&p.window)
    }
    #[profiling::function]
    fn refresh(&self) {
        let p = self.p();
        SpaceElement::refresh(&p.window);
        if p.has_ssd(true) {
            let title = p.window.title();
            let mut last_title = p.last_title.lock().unwrap();
            if *last_title != title {
                *last_title = title;
            }
        }
    }
}

impl KeyboardTarget<State> for CosmicWindow {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        let p = self.p();
        KeyboardTarget::enter(&p.window, seat, data, keys, serial)
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        let p = self.p();
        KeyboardTarget::leave(&p.window, seat, data, serial)
    }
    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        let p = self.p();
        KeyboardTarget::key(&p.window, seat, data, key, state, serial, time)
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        let p = self.p();
        KeyboardTarget::modifiers(&p.window, seat, data, modifiers, serial)
    }
}

impl PointerTarget<State> for CosmicWindow {
    fn enter(&self, seat: &Seat<State>, _data: &mut State, event: &MotionEvent) {
        let p = self.p();
        let has_ssd = p.has_ssd(false);
        if has_ssd || p.has_tiled_state() {
            let Some(next) = Focus::under(
                &p.window,
                if has_ssd { SSD_HEIGHT } else { 0 },
                event.location,
            ) else {
                return;
            };

            let old_focus = p.swap_focus(Some(next));
            assert_eq!(old_focus, None);

            let cursor_state = seat.user_data().get::<CursorState>().unwrap();
            cursor_state.lock().unwrap().set_shape(next.cursor_shape());
            seat.set_cursor_image_status(CursorImageStatus::default_named());
        }
    }

    fn motion(&self, seat: &Seat<State>, _data: &mut State, event: &MotionEvent) {
        // Disarm the double-click tracker if the pointer has moved
        // far enough to look like an intentional drag between clicks.
        // Cheap no-op when the tracker has no baseline.
        seat.double_click_tracker().invalidate_on_motion(event.location);

        // Feature 4-C: update the window-control button interaction
        // state so the next frame's header rasterisation picks the
        // right hover / pressed visuals. Only meaningful in the SSD
        // header strip — `update_header_button_interaction` handles
        // the "not in header" case by settling on `Idle`.
        self.update_header_button_interaction(event.location);

        let p = self.p();
        let has_ssd = p.has_ssd(false);
        if has_ssd || p.has_tiled_state() {
            let Some(next) = Focus::under(
                &p.window,
                if has_ssd { SSD_HEIGHT } else { 0 },
                event.location,
            ) else {
                return;
            };
            let _previous = p.swap_focus(Some(next));

            let cursor_state = seat.user_data().get::<CursorState>().unwrap();
            cursor_state.lock().unwrap().set_shape(next.cursor_shape());
            seat.set_cursor_image_status(CursorImageStatus::default_named());
        }
    }

    fn relative_motion(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &RelativeMotionEvent,
    ) {
    }

    fn button(&self, seat: &Seat<State>, _data: &mut State, event: &ButtonEvent) {
        let current_focus = self.p().current_focus();
        tracing::info!(
            "CosmicWindow::button focus={:?} button=0x{:x} state={:?}",
            current_focus, event.button, event.state,
        );
        match current_focus {
            Some(Focus::Header) => {
                // Left release: if we were pressing a header button
                // (min/max/close), invoke the corresponding action
                // instead of falling through to any default handler.
                // Release of the right or middle button is ignored.
                if event.state == smithay::backend::input::ButtonState::Released
                    && event.button == 0x110
                {
                    if let Some(button) = self.finalize_header_button_release() {
                        use crate::backend::render::window_header::HeaderButton;
                        tracing::info!(
                            "HEADER-BTN-DEBUG release fired={:?} (compositor-rendered)",
                            button
                        );
                        let seat = seat.clone();
                        let surface_opt = {
                            let p = self.p();
                            p.window.wl_surface().map(Cow::into_owned)
                        };
                        if let Some(surface) = surface_opt {
                            self.handle.insert_idle(move |state| {
                                let mapped = {
                                    let shell_r = state.common.shell.read();
                                    shell_r.element_for_surface(&surface).cloned()
                                };
                                let Some(mapped) = mapped else { return };
                                match button {
                                    HeaderButton::Minimize => {
                                        let info = state
                                            .common
                                            .shell
                                            .write()
                                            .minimize_request(&mapped.active_window());
                                        if let Some(info) = info {
                                            state.common.event_bus.emit_window_minimized(
                                                &info.window_id, &info.app_id,
                                                &info.title, &info.workspace_id,
                                            );
                                        }
                                    }
                                    HeaderButton::Maximize => {
                                        state.common.shell.write().maximize_toggle(
                                            &mapped, &seat,
                                            &state.common.event_loop_handle,
                                        );
                                    }
                                    HeaderButton::Close => {
                                        mapped.active_window().close();
                                    }
                                }
                            });
                        }
                        return;
                    }
                }

                // Left press: drag start OR, if this is the second
                // press of a double-click, maximize-toggle instead.
                // Release and repeat presses of non-left buttons fall
                // through the tracker below without altering it.
                if event.state == smithay::backend::input::ButtonState::Pressed
                    && event.button == 0x110
                {
                    // Feature 4-C: if the pointer is currently over
                    // a window-control button, flip the interaction
                    // to Pressed and intercept — no move grab, no
                    // double-click tracking. The click action fires
                    // on release in the branch above.
                    if let Some(button) = self.arm_header_button_press_from_hover() {
                        tracing::info!(
                            "HEADER-BTN-DEBUG press armed {:?} (compositor-rendered)",
                            button
                        );
                        return;
                    }

                    let serial = event.serial;
                    // NOTE: do NOT read `seat.get_pointer().current_location()`
                    // synchronously here. Smithay dispatches
                    // `PointerTarget::button` with the pointer's outer
                    // `Mutex` held; any re-entrant `current_location()`
                    // call deadlocks the whole compositor. Pointer
                    // distance for the double-click is enforced via the
                    // motion handler's `invalidate_on_motion` instead.
                    let surface_opt = {
                        let p = self.p();
                        p.window.wl_surface().map(Cow::into_owned)
                    };
                    if let Some(surface) = surface_opt {
                        use smithay::reexports::wayland_server::Resource;
                        let target_id = surface.id().protocol_id() as u64;
                        let is_double = seat.double_click_tracker().observe_press(
                            event.time,
                            event.button,
                            target_id,
                        );
                        tracing::info!(
                            "DCLK-DEBUG CosmicWindow header press time={} \
                             button=0x{:x} target_id={} double={}",
                            event.time, event.button, target_id, is_double
                        );
                        if is_double {
                            let seat = seat.clone();
                            let surface_cloned = surface.clone();
                            self.handle.insert_idle(move |state| {
                                let mapped = {
                                    let shell_r = state.common.shell.read();
                                    shell_r.element_for_surface(&surface_cloned).cloned()
                                };
                                if let Some(mapped) = mapped {
                                    tracing::info!(
                                        "DCLK-DEBUG maximize_toggle target_id={}",
                                        target_id
                                    );
                                    state.common.shell.write().maximize_toggle(
                                        &mapped,
                                        &seat,
                                        &state.common.event_loop_handle,
                                    );
                                }
                            });
                        } else {
                            let seat = seat.clone();
                            let surface = surface.clone();
                            self.handle.insert_idle(move |state| {
                                let res = state.common.shell.write().move_request(
                                    &surface,
                                    &seat,
                                    serial,
                                    ReleaseMode::NoMouseButtons,
                                    false,
                                    &state.common.config,
                                    &state.common.event_loop_handle,
                                    false,
                                );
                                if let Some((grab, focus)) = res {
                                    // Feature 4: emit drag_start
                                    // synchronously BEFORE installing
                                    // the grab so the shell gets it
                                    // before any pointer-motion
                                    // header-update event. If we
                                    // routed this through
                                    // `insert_idle` again, motion
                                    // events could race ahead and the
                                    // shell would process the first
                                    // update with CSS transitions
                                    // still enabled — that's the
                                    // "lag at drag start" symptom.
                                    if let Some(sid) = grab.drag_surface_id() {
                                        tracing::info!(
                                            "ATTACH-DEBUG drag_start surface_id={}",
                                            sid
                                        );
                                        state
                                            .common
                                            .shell_overlay_state
                                            .send_window_drag_start(sid);
                                    }
                                    if grab.is_touch_grab() {
                                        seat.get_touch().unwrap().set_grab(state, grab, serial);
                                    } else {
                                        seat.get_pointer().unwrap().set_grab(
                                            state, grab, serial, focus,
                                        );
                                    }
                                }
                            });
                        }
                    }
                }
                // Right click: context menu
                if event.state == smithay::backend::input::ButtonState::Pressed
                    && event.button == 0x111
                {
                    let serial = event.serial;
                    let seat = seat.clone();
                    let surface = {
                        let p = self.p();
                        p.window.wl_surface().map(Cow::into_owned)
                    };
                    if let Some(surface) = surface {
                        self.handle.insert_idle(move |state| {
                            let shell = state.common.shell.read();
                            if let Some(mapped) = shell.element_for_surface(&surface).cloned() {
                                let position = if let Some((output, set)) =
                                    shell.workspaces.sets.iter().find(|(_, set)| {
                                        set.sticky_layer.mapped().any(|m| m == &mapped)
                                    }) {
                                    set.sticky_layer
                                        .element_geometry(&mapped)
                                        .unwrap()
                                        .loc
                                        .to_global(output)
                                } else if let Some(workspace) = shell.space_for(&mapped) {
                                    let Some(elem_geo) = workspace.element_geometry(&mapped) else {
                                        return;
                                    };
                                    elem_geo.loc.to_global(&workspace.output)
                                } else {
                                    return;
                                };

                                let pointer = seat.get_pointer().unwrap();
                                let mut cursor = pointer.current_location().to_i32_round();
                                cursor.y -= SSD_HEIGHT;

                                let res = shell.menu_request(
                                    false,
                                    &surface,
                                    &seat,
                                    serial,
                                    cursor - position.as_logical(),
                                    false,
                                    &state.common.config,
                                    &state.common.event_loop_handle,
                                    &state.common.display_handle,
                                    &mut state.common.shell_overlay_state,
                                    &mut state.common.pending_menu_callbacks,
                                );

                                std::mem::drop(shell);
                                if let Some((grab, focus)) = res {
                                    pointer.set_grab(state, grab, serial, focus);
                                }
                            }
                        });
                    }
                }
            }
            Some(x) => {
                let serial = event.serial;
                let seat = seat.clone();
                let Some(surface) = self.wl_surface().map(Cow::into_owned) else {
                    return;
                };

                // Only start a resize if the left button was pressed
                if event.state != smithay::backend::input::ButtonState::Pressed
                    || event.button != 0x110
                {
                    return;
                }
                self.handle.insert_idle(move |state| {
                    let res = state.common.shell.write().resize_request(
                        &surface,
                        &seat,
                        serial,
                        match x {
                            Focus::ResizeTop => ResizeEdge::TOP,
                            Focus::ResizeTopLeft => ResizeEdge::TOP_LEFT,
                            Focus::ResizeTopRight => ResizeEdge::TOP_RIGHT,
                            Focus::ResizeBottom => ResizeEdge::BOTTOM,
                            Focus::ResizeBottomLeft => ResizeEdge::BOTTOM_LEFT,
                            Focus::ResizeBottomRight => ResizeEdge::BOTTOM_RIGHT,
                            Focus::ResizeLeft => ResizeEdge::LEFT,
                            Focus::ResizeRight => ResizeEdge::RIGHT,
                            Focus::Header => unreachable!(),
                        },
                        state.common.config.cosmic_conf.edge_snap_threshold,
                        false,
                    );

                    if let Some((grab, focus)) = res {
                        if grab.is_touch_grab() {
                            seat.get_touch().unwrap().set_grab(state, grab, serial);
                        } else {
                            seat.get_pointer()
                                .unwrap()
                                .set_grab(state, grab, serial, focus);
                        }
                    }
                });
            }
            None => {}
        }
    }

    fn axis(&self, _seat: &Seat<State>, _data: &mut State, _frame: AxisFrame) {
        // No-op for header (Iced header scrolling removed).
    }

    fn frame(&self, _seat: &Seat<State>, _data: &mut State) {
        // No-op for header.
    }

    fn leave(&self, seat: &Seat<State>, _data: &mut State, _serial: Serial, _time: u32) {
        // Pointer left the window entirely; any armed double-click
        // baseline is moot. Clear it so the next window's first click
        // doesn't accidentally cash in on this one's timestamp.
        seat.double_click_tracker().invalidate();

        let p = self.p();
        let cursor_state = seat.user_data().get::<CursorState>().unwrap();
        cursor_state.lock().unwrap().unset_shape();
        let _previous = p.swap_focus(None);
    }

    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GestureSwipeBeginEvent,
    ) {
    }

    fn gesture_swipe_update(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }

    fn gesture_swipe_end(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GestureSwipeEndEvent,
    ) {
    }

    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GesturePinchBeginEvent,
    ) {
    }

    fn gesture_pinch_update(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GesturePinchUpdateEvent,
    ) {
    }

    fn gesture_pinch_end(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GesturePinchEndEvent,
    ) {
    }

    fn gesture_hold_begin(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GestureHoldBeginEvent,
    ) {
    }

    fn gesture_hold_end(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl TouchTarget<State> for CosmicWindow {
    fn down(&self, _seat: &Seat<State>, _data: &mut State, event: &DownEvent, _seq: Serial) {
        let _adjusted_loc = {
            let p = self.p();
            event.location - p.window.geometry().loc.to_f64()
        };
    }

    fn up(&self, _seat: &Seat<State>, _data: &mut State, _event: &UpEvent, _seq: Serial) {
    }

    fn motion(&self, _seat: &Seat<State>, _data: &mut State, event: &TouchMotionEvent, _seq: Serial) {
        let _adjusted_loc = {
            let p = self.p();
            event.location - p.window.geometry().loc.to_f64()
        };
    }

    fn frame(&self, _seat: &Seat<State>, _data: &mut State, _seq: Serial) {
    }

    fn cancel(&self, _seat: &Seat<State>, _data: &mut State, _seq: Serial) {
    }

    fn shape(&self, _seat: &Seat<State>, _data: &mut State, _event: &ShapeEvent, _seq: Serial) {
    }

    fn orientation(
        &self,
        _seat: &Seat<State>,
        _data: &mut State,
        _event: &OrientationEvent,
        _seq: Serial,
    ) {
    }
}

impl WaylandFocus for CosmicWindow {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        let p = self.p();
        p.window
            .wl_surface()
            .map(|s| Cow::Owned(Cow::into_owned(s)))
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        self.p().window.same_client_as(object_id)
    }
}

/// Render element variants for a CosmicWindow.
pub enum CosmicWindowRenderElement<R: Renderer + ImportAll + ImportMem> {
    Header(MemoryRenderBufferRenderElement<R>),
    Shadow(PixelShaderElement),
    Border(PixelShaderElement),
    Window(WaylandSurfaceRenderElement<R>),
    Clipped(ClippedSurfaceRenderElement<R>),
}

impl<R: Renderer + ImportAll + ImportMem> From<MemoryRenderBufferRenderElement<R>>
    for CosmicWindowRenderElement<R>
{
    fn from(value: MemoryRenderBufferRenderElement<R>) -> Self {
        Self::Header(value)
    }
}

impl<R: Renderer + ImportAll + ImportMem> From<WaylandSurfaceRenderElement<R>>
    for CosmicWindowRenderElement<R>
{
    fn from(value: WaylandSurfaceRenderElement<R>) -> Self {
        Self::Window(value)
    }
}

impl<R: Renderer + ImportAll + ImportMem> From<ClippedSurfaceRenderElement<R>>
    for CosmicWindowRenderElement<R>
{
    fn from(value: ClippedSurfaceRenderElement<R>) -> Self {
        Self::Clipped(value)
    }
}

impl<R> Element for CosmicWindowRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem,
{
    fn id(&self) -> &RendererId {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.id(),
            CosmicWindowRenderElement::Shadow(elem) => elem.id(),
            CosmicWindowRenderElement::Border(elem) => elem.id(),
            CosmicWindowRenderElement::Window(elem) => elem.id(),
            CosmicWindowRenderElement::Clipped(elem) => elem.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.current_commit(),
            CosmicWindowRenderElement::Shadow(elem) => elem.current_commit(),
            CosmicWindowRenderElement::Border(elem) => elem.current_commit(),
            CosmicWindowRenderElement::Window(elem) => elem.current_commit(),
            CosmicWindowRenderElement::Clipped(elem) => elem.current_commit(),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.src(),
            CosmicWindowRenderElement::Shadow(elem) => elem.src(),
            CosmicWindowRenderElement::Border(elem) => elem.src(),
            CosmicWindowRenderElement::Window(elem) => elem.src(),
            CosmicWindowRenderElement::Clipped(elem) => elem.src(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.geometry(scale),
            CosmicWindowRenderElement::Shadow(elem) => elem.geometry(scale),
            CosmicWindowRenderElement::Border(elem) => elem.geometry(scale),
            CosmicWindowRenderElement::Window(elem) => elem.geometry(scale),
            CosmicWindowRenderElement::Clipped(elem) => elem.geometry(scale),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.location(scale),
            CosmicWindowRenderElement::Shadow(elem) => elem.location(scale),
            CosmicWindowRenderElement::Border(elem) => elem.location(scale),
            CosmicWindowRenderElement::Window(elem) => elem.location(scale),
            CosmicWindowRenderElement::Clipped(elem) => elem.location(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.transform(),
            CosmicWindowRenderElement::Shadow(elem) => elem.transform(),
            CosmicWindowRenderElement::Border(elem) => elem.transform(),
            CosmicWindowRenderElement::Window(elem) => elem.transform(),
            CosmicWindowRenderElement::Clipped(elem) => elem.transform(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.damage_since(scale, commit),
            CosmicWindowRenderElement::Shadow(elem) => elem.damage_since(scale, commit),
            CosmicWindowRenderElement::Border(elem) => elem.damage_since(scale, commit),
            CosmicWindowRenderElement::Window(elem) => elem.damage_since(scale, commit),
            CosmicWindowRenderElement::Clipped(elem) => elem.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.opaque_regions(scale),
            CosmicWindowRenderElement::Shadow(elem) => elem.opaque_regions(scale),
            CosmicWindowRenderElement::Border(elem) => elem.opaque_regions(scale),
            CosmicWindowRenderElement::Window(elem) => elem.opaque_regions(scale),
            CosmicWindowRenderElement::Clipped(elem) => elem.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.alpha(),
            CosmicWindowRenderElement::Shadow(elem) => elem.alpha(),
            CosmicWindowRenderElement::Border(elem) => elem.alpha(),
            CosmicWindowRenderElement::Window(elem) => elem.alpha(),
            CosmicWindowRenderElement::Clipped(elem) => elem.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.kind(),
            CosmicWindowRenderElement::Shadow(elem) => elem.kind(),
            CosmicWindowRenderElement::Border(elem) => elem.kind(),
            CosmicWindowRenderElement::Window(elem) => elem.kind(),
            CosmicWindowRenderElement::Clipped(elem) => elem.kind(),
        }
    }
}

impl<R> RenderElement<R> for CosmicWindowRenderElement<R>
where
    R: AsGlowRenderer,
    R::TextureId: 'static,
    R::Error: FromGlesError,
{
    fn draw(
        &self,
        frame: &mut <R>::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), <R>::Error> {
        match self {
            CosmicWindowRenderElement::Header(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            CosmicWindowRenderElement::Shadow(elem) | CosmicWindowRenderElement::Border(elem) => {
                RenderElement::<GlowRenderer>::draw(
                    elem,
                    R::glow_frame_mut(frame),
                    src,
                    dst,
                    damage,
                    opaque_regions,
                )
                .map_err(FromGlesError::from_gles_error)
            }
            CosmicWindowRenderElement::Window(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            CosmicWindowRenderElement::Clipped(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
        }
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        match self {
            CosmicWindowRenderElement::Header(elem) => elem.underlying_storage(renderer),
            CosmicWindowRenderElement::Shadow(elem) | CosmicWindowRenderElement::Border(elem) => {
                elem.underlying_storage(renderer.glow_renderer_mut())
            }
            CosmicWindowRenderElement::Window(elem) => elem.underlying_storage(renderer),
            CosmicWindowRenderElement::Clipped(elem) => elem.underlying_storage(renderer),
        }
    }
}
