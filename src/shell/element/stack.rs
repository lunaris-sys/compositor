use super::{
    CosmicSurface,
    window::{Focus, RESIZE_BORDER},
};
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
        layout::tiling::NodeDesc,
    },
    state::State,
    utils::prelude::*,
};
use calloop::LoopHandle;
use cosmic_comp_config::AppearanceConfig;
use cosmic_settings_config::shortcuts;
use shortcuts::action::{Direction, FocusDirection};
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
            AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
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
        atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering},
    },
};


static NEXT_STACK_ID: AtomicU32 = AtomicU32::new(1);

/// A stack of windows displayed with a tab bar, managed by the shell overlay protocol.
#[derive(Clone)]
pub struct CosmicStack {
    pub(super) inner: Arc<Mutex<CosmicStackInternal>>,
    handle: LoopHandle<'static, crate::state::State>,
}

// SAFETY: LoopHandle contains an Rc internally, making CosmicStack !Send/!Sync by
// default. All LoopHandle usage is on the main thread (calloop event loop), and the
// interior state is serialised through the Mutex. This mirrors IcedElementInternal.
unsafe impl Send for CosmicStack {}
unsafe impl Sync for CosmicStack {}

impl PartialEq for CosmicStack {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CosmicStack {}

impl Hash for CosmicStack {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.inner).hash(state);
    }
}

impl fmt::Debug for CosmicStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CosmicStack")
            .field("inner", &Arc::as_ptr(&self.inner))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct CosmicStackInternal {
    /// Unique identifier for this stack, used in the shell overlay protocol.
    stack_id: u32,
    windows: Mutex<Vec<CosmicSurface>>,
    active: AtomicUsize,
    activated: AtomicBool,
    group_focused: AtomicBool,
    previous_index: Mutex<Option<(Serial, usize)>>,
    scroll_to_focus: AtomicBool,
    previous_keyboard: AtomicUsize,
    pointer_entered: AtomicU8,
    reenter: AtomicBool,
    potential_drag: Mutex<Option<usize>>,
    override_alive: AtomicBool,
    geometry: Mutex<Option<Rectangle<i32, Global>>>,
    mask: Mutex<Option<tiny_skia::Mask>>,
    tiled: AtomicBool,
    appearance_conf: Mutex<AppearanceConfig>,
}

impl CosmicStackInternal {
    pub fn swap_focus(&self, focus: Option<Focus>) -> Option<Focus> {
        let value = focus.map_or(0, |x| x as u8);
        unsafe { Focus::from_u8(self.pointer_entered.swap(value, Ordering::SeqCst)) }
    }

    pub fn current_focus(&self) -> Option<Focus> {
        unsafe { Focus::from_u8(self.pointer_entered.load(Ordering::SeqCst)) }
    }
}

pub const TAB_HEIGHT: i32 = 24;

#[derive(Debug, Clone)]
pub enum MoveResult {
    Handled,
    MoveOut(CosmicSurface, LoopHandle<'static, crate::state::State>),
    Default,
}

impl CosmicStack {
    /// Helper to lock the internal state.
    fn p(&self) -> MutexGuard<'_, CosmicStackInternal> {
        self.inner.lock().unwrap()
    }

    pub fn new<I: Into<CosmicSurface>>(
        windows: impl Iterator<Item = I>,
        handle: LoopHandle<'static, crate::state::State>,
        appearance: AppearanceConfig,
    ) -> CosmicStack {
        let windows = windows.map(Into::into).collect::<Vec<_>>();
        assert!(!windows.is_empty());

        for window in &windows {
            window.try_force_undecorated(true);
            window.set_tiled(true);
            window.send_configure();
        }

        let stack_id = NEXT_STACK_ID.fetch_add(1, Ordering::Relaxed);
        // Notify shell about the initial tab list.
        let initial_tabs: Vec<_> = windows
            .iter()
            .enumerate()
            .map(|(i, w)| (i as u32, w.title(), w.app_id()))
            .collect();
        handle.insert_idle(move |state| {
            for (index, title, app_id) in initial_tabs {
                state.common.shell_overlay_state.send_tab_added(
                    stack_id,
                    index,
                    title,
                    app_id,
                    index == 0,
                );
            }
        });

        CosmicStack {
            inner: Arc::new(Mutex::new(CosmicStackInternal {
                stack_id,
                windows: Mutex::new(windows),
                active: AtomicUsize::new(0),
                activated: AtomicBool::new(false),
                group_focused: AtomicBool::new(false),
                previous_index: Mutex::new(None),
                scroll_to_focus: AtomicBool::new(false),
                previous_keyboard: AtomicUsize::new(0),
                pointer_entered: AtomicU8::new(0),
                reenter: AtomicBool::new(false),
                potential_drag: Mutex::new(None),
                override_alive: AtomicBool::new(true),
                geometry: Mutex::new(None),
                mask: Mutex::new(None),
                tiled: AtomicBool::new(false),
                appearance_conf: Mutex::new(appearance),
            })),
            handle,
        }
    }

    pub fn add_window(
        &self,
        window: impl Into<CosmicSurface>,
        idx: Option<usize>,
        moved_into: Option<&Seat<State>>,
    ) {
        let window = window.into();
        window.try_force_undecorated(true);
        window.set_tiled(true);
        let tab_event = {
            let p = self.p();
            let last_mod_serial = moved_into.and_then(|seat| seat.last_modifier_change());
            let mut prev_idx = p.previous_index.lock().unwrap();
            if !prev_idx.is_some_and(|(serial, _)| Some(serial) == last_mod_serial) {
                *prev_idx = last_mod_serial.map(|s| (s, p.active.load(Ordering::SeqCst)));
            }

            if let Some(mut geo) = *p.geometry.lock().unwrap() {
                geo.loc.y += TAB_HEIGHT;
                geo.size.h -= TAB_HEIGHT;
                window.set_geometry(geo, TAB_HEIGHT as u32);
            }
            window.send_configure();
            let (final_idx, is_active) = if let Some(idx) = idx {
                p.windows.lock().unwrap().insert(idx, window.clone());
                let old_idx = p.active.swap(idx, Ordering::SeqCst);
                if old_idx == idx {
                    p.reenter.store(true, Ordering::SeqCst);
                    p.previous_keyboard.store(old_idx, Ordering::SeqCst);
                }
                (idx, true)
            } else {
                let mut windows = p.windows.lock().unwrap();
                windows.push(window.clone());
                let new_idx = windows.len() - 1;
                p.active.store(new_idx, Ordering::SeqCst);
                (new_idx, true)
            };
            p.scroll_to_focus.store(true, Ordering::SeqCst);
            (p.stack_id, final_idx as u32, window.title(), window.app_id(), is_active)
        };
        // Notify shell about the new tab.
        let (stack_id, index, title, app_id, active) = tab_event;
        self.handle.insert_idle(move |state| {
            state.common.shell_overlay_state.send_tab_added(stack_id, index, title, app_id, active);
        });
    }

    pub fn remove_window(&self, window: &CosmicSurface) {
        let tab_event = {
            let p = self.p();
            let mut windows = p.windows.lock().unwrap();
            if windows.len() == 1 {
                p.override_alive.store(false, Ordering::SeqCst);
                let window = windows.first().unwrap();
                window.try_force_undecorated(false);
                window.set_tiled(false);
                None
            } else {
                let Some(idx) = windows.iter().position(|w| w == window) else {
                    return;
                };
                if idx == p.active.load(Ordering::SeqCst) {
                    p.reenter.store(true, Ordering::SeqCst);
                }
                let window = windows.remove(idx);
                window.try_force_undecorated(false);
                window.set_tiled(false);

                p.active.fetch_min(windows.len() - 1, Ordering::SeqCst);
                Some((p.stack_id, idx as u32))
            }
        };
        if let Some((stack_id, index)) = tab_event {
            self.handle.insert_idle(move |state| {
                state.common.shell_overlay_state.send_tab_removed(stack_id, index);
            });
        }
    }

    pub fn remove_idx(&self, idx: usize) -> Option<CosmicSurface> {
        let (window, tab_event) = {
            let p = self.p();
            let mut windows = p.windows.lock().unwrap();
            if windows.len() == 1 {
                p.override_alive.store(false, Ordering::SeqCst);
                let window = windows.first().unwrap();
                window.try_force_undecorated(false);
                window.set_tiled(false);
                (Some(window.clone()), None)
            } else if windows.len() <= idx {
                (None, None)
            } else {
                if idx == p.active.load(Ordering::SeqCst) {
                    p.reenter.store(true, Ordering::SeqCst);
                }
                let window = windows.remove(idx);
                window.try_force_undecorated(false);
                window.set_tiled(false);

                p.active.fetch_min(windows.len() - 1, Ordering::SeqCst);

                (Some(window), Some((p.stack_id, idx as u32)))
            }
        };
        if let Some((stack_id, index)) = tab_event {
            self.handle.insert_idle(move |state| {
                state.common.shell_overlay_state.send_tab_removed(stack_id, index);
            });
        }
        window
    }

    /// Returns the unique stack identifier used in the shell overlay protocol.
    pub fn stack_id(&self) -> u32 {
        let p = self.p();
        p.stack_id
    }

    pub fn len(&self) -> usize {
        let p = self.p();
        p.windows.lock().unwrap().len()
    }

    pub fn handle_focus(
        &self,
        seat: &Seat<State>,
        direction: FocusDirection,
        swap: Option<NodeDesc>,
    ) -> bool {
        let (result, _update) = {
            let p = self.p();
            let last_mod_serial = seat.last_modifier_change();
            let mut prev_idx = p.previous_index.lock().unwrap();
            if !prev_idx.is_some_and(|(serial, _)| Some(serial) == last_mod_serial) {
                *prev_idx = last_mod_serial.map(|s| (s, p.active.load(Ordering::SeqCst)));
            }

            match direction {
                FocusDirection::Left => {
                    if !p.group_focused.load(Ordering::SeqCst) {
                        if let Ok(old) =
                            p.active
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                                    val.checked_sub(1)
                                })
                        {
                            p.previous_keyboard.store(old, Ordering::SeqCst);
                            p.scroll_to_focus.store(true, Ordering::SeqCst);
                            (true, true)
                        } else {
                            let new = prev_idx.unwrap().1;
                            let old = p.active.swap(new, Ordering::SeqCst);
                            if old != new {
                                p.previous_keyboard.store(old, Ordering::SeqCst);
                                p.scroll_to_focus.store(true, Ordering::SeqCst);
                                (false, true)
                            } else {
                                (false, false)
                            }
                        }
                    } else {
                        (false, false)
                    }
                }
                FocusDirection::Right => {
                    if !p.group_focused.load(Ordering::SeqCst) {
                        let max = p.windows.lock().unwrap().len();
                        if let Ok(old) =
                            p.active
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                                    if val < max - 1 { Some(val + 1) } else { None }
                                })
                        {
                            p.previous_keyboard.store(old, Ordering::SeqCst);
                            p.scroll_to_focus.store(true, Ordering::SeqCst);
                            (true, true)
                        } else {
                            let new = prev_idx.unwrap().1;
                            let old = p.active.swap(new, Ordering::SeqCst);
                            if old != new {
                                p.previous_keyboard.store(old, Ordering::SeqCst);
                                p.scroll_to_focus.store(true, Ordering::SeqCst);
                                (false, true)
                            } else {
                                (false, false)
                            }
                        }
                    } else {
                        (false, false)
                    }
                }
                FocusDirection::Out if swap.is_none() => {
                    if !p.group_focused.swap(true, Ordering::SeqCst) {
                        p.windows.lock().unwrap().iter().for_each(|w| {
                            w.set_activated(false);
                            w.send_configure();
                        });
                        (true, true)
                    } else {
                        (false, false)
                    }
                }
                FocusDirection::In if swap.is_none() => {
                    if !p.group_focused.swap(false, Ordering::SeqCst) {
                        p.windows
                            .lock()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .for_each(|(i, w)| {
                                w.set_activated(p.active.load(Ordering::SeqCst) == i);
                                w.send_configure();
                            });

                        (true, true)
                    } else {
                        (false, false)
                    }
                }
                FocusDirection::Up | FocusDirection::Down => {
                    if !p.group_focused.load(Ordering::SeqCst) {
                        let new = prev_idx.unwrap().1;
                        let old = p.active.swap(new, Ordering::SeqCst);
                        if old != new {
                            p.previous_keyboard.store(old, Ordering::SeqCst);
                            p.scroll_to_focus.store(true, Ordering::SeqCst);
                        }
                        (false, true)
                    } else {
                        (false, false)
                    }
                }
                _ => (false, false),
            }
        };

        result
    }

    pub fn handle_move(&self, direction: Direction) -> MoveResult {
        let loop_handle = self.handle.clone();
        let result = {
            let p = self.p();
            let prev_idx = p.previous_index.lock().unwrap();

            if p.group_focused.load(Ordering::SeqCst) {
                MoveResult::Default
            } else {
                let active = p.active.load(Ordering::SeqCst);
                let mut windows = p.windows.lock().unwrap();

                let next = match direction {
                    Direction::Left => active.checked_sub(1),
                    Direction::Right => (active + 1 < windows.len()).then_some(active + 1),
                    Direction::Down | Direction::Up => None,
                };

                if let Some(val) = next {
                    let old = p.active.swap(val, Ordering::SeqCst);
                    windows.swap(old, val);
                    p.previous_keyboard.store(old, Ordering::SeqCst);
                    p.scroll_to_focus.store(true, Ordering::SeqCst);
                    MoveResult::Handled
                } else {
                    if windows.len() == 1 {
                        MoveResult::Default
                    } else {
                        let window = windows.remove(active);
                        if let Some(prev_idx) = prev_idx
                            .map(|(_, idx)| idx)
                            .filter(|idx| *idx < windows.len())
                        {
                            p.active.store(prev_idx, Ordering::SeqCst);
                            p.scroll_to_focus.store(true, Ordering::SeqCst);
                        } else if active == windows.len() {
                            p.active.store(active - 1, Ordering::SeqCst);
                            p.scroll_to_focus.store(true, Ordering::SeqCst);
                        }
                        window.try_force_undecorated(false);
                        window.set_tiled(false);

                        MoveResult::MoveOut(window, loop_handle)
                    }
                }
            }
        };

        result
    }

    pub fn active(&self) -> CosmicSurface {
        let p = self.p();
        p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].clone()
    }

    pub fn has_active(&self, window: &CosmicSurface) -> bool {
        let p = self.p();
        &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)] == window
    }

    pub fn whole_stack_focused(&self) -> bool {
        let p = self.p();
        p.group_focused.load(Ordering::SeqCst)
    }

    pub fn set_active<S>(&self, window: &S)
    where
        CosmicSurface: PartialEq<S>,
    {
        let tab_event = {
            let p = self.p();
            if let Some(val) = p.windows.lock().unwrap().iter().position(|w| w == window) {
                let old = p.active.swap(val, Ordering::SeqCst);
                if old != val {
                    p.previous_keyboard.store(old, Ordering::SeqCst);
                    Some((p.stack_id, val as u32))
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some((stack_id, index)) = tab_event {
            self.handle.insert_idle(move |state| {
                state.common.shell_overlay_state.send_tab_activated(stack_id, index);
            });
        }
    }

    pub fn set_tiled(&self, tiled: bool) {
        let p = self.p();
        p.tiled.store(tiled, Ordering::Release);
    }

    pub fn surfaces(&self) -> impl Iterator<Item = CosmicSurface> {
        let p = self.p();
        p.windows
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn focus_under(
        &self,
        mut relative_pos: Point<f64, Logical>,
        surface_type: WindowSurfaceType,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        let p = self.p();
        let mut stack_ui = None;
        let geo = p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].geometry();

        if surface_type.contains(WindowSurfaceType::TOPLEVEL) {
            let point_i32 = relative_pos.to_i32_round::<i32>();
            if (point_i32.x - geo.loc.x >= -RESIZE_BORDER && point_i32.x - geo.loc.x < 0)
                || (point_i32.y - geo.loc.y >= -RESIZE_BORDER && point_i32.y - geo.loc.y < 0)
                || (point_i32.x - geo.loc.x >= geo.size.w
                    && point_i32.x - geo.loc.x < geo.size.w + RESIZE_BORDER)
                || (point_i32.y - geo.loc.y >= geo.size.h + TAB_HEIGHT
                    && point_i32.y - geo.loc.y < geo.size.h + TAB_HEIGHT + RESIZE_BORDER)
            {
                stack_ui = Some((
                    PointerFocusTarget::StackUI(self.clone()),
                    Point::from((0., 0.)),
                ));
            }

            if point_i32.y - geo.loc.y < TAB_HEIGHT {
                stack_ui = Some((
                    PointerFocusTarget::StackUI(self.clone()),
                    Point::from((0., 0.)),
                ));
            }
        }

        relative_pos.y -= TAB_HEIGHT as f64;

        let active_window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
        stack_ui.or_else(|| {
            active_window.focus_under(relative_pos, surface_type).map(
                |(target, surface_offset)| {
                    (
                        target,
                        surface_offset.to_f64() + Point::from((0., TAB_HEIGHT as f64)),
                    )
                },
            )
        })
    }

    pub fn offset(&self) -> Point<i32, Logical> {
        Point::from((0, TAB_HEIGHT))
    }

    pub fn pending_size(&self) -> Option<Size<i32, Logical>> {
        let p = self.p();
        (*p.geometry.lock().unwrap()).map(|geo| geo.size.as_logical())
    }

    pub fn set_geometry(&self, geo: Rectangle<i32, Global>) {
        let p = self.p();
        let loc = (geo.loc.x, geo.loc.y + TAB_HEIGHT);
        let size = (geo.size.w, geo.size.h - TAB_HEIGHT);

        let win_geo = Rectangle::new(loc.into(), size.into());
        for window in p.windows.lock().unwrap().iter() {
            window.set_geometry(win_geo, TAB_HEIGHT as u32);
        }

        *p.geometry.lock().unwrap() = Some(geo);
        p.mask.lock().unwrap().take();
    }

    pub fn on_commit(&self, surface: &WlSurface) {
        if let Some(surface) = self.surfaces().find(|w| w == surface) {
            surface.0.on_commit();
        }
    }

    fn keyboard_leave_if_previous(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        serial: Serial,
    ) -> usize {
        let p = self.p();
        let active = p.active.load(Ordering::SeqCst);
        let previous = p.previous_keyboard.swap(active, Ordering::SeqCst);
        if previous != active || p.reenter.swap(false, Ordering::SeqCst) {
            let windows = p.windows.lock().unwrap();
            if let Some(previous_surface) = windows.get(previous)
                && previous != active
            {
                KeyboardTarget::leave(previous_surface, seat, data, serial);
            }
            KeyboardTarget::enter(
                &windows[active],
                seat,
                data,
                Vec::new(), /* TODO */
                serial,
            )
        }
        active
    }

    pub(in super::super) fn focus_stack(&self) {
        let p = self.p();
        p.group_focused.store(true, Ordering::SeqCst);
    }

    pub(in super::super) fn loop_handle(&self) -> LoopHandle<'static, crate::state::State> {
        self.handle.clone()
    }

    pub fn popup_render_elements<R, C>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C>
    where
        R: AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        C: From<CosmicStackRenderElement<R>>,
    {
        let window_loc = location + Point::from((0, (TAB_HEIGHT as f64 * scale.y) as i32));
        let p = self.p();
        let windows = p.windows.lock().unwrap();
        let active = p.active.load(Ordering::SeqCst);

        windows[active]
            .popup_render_elements::<R, CosmicStackRenderElement<R>>(
                renderer, window_loc, scale, alpha,
            )
            .into_iter()
            .map(C::from)
            .collect()
    }

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
        C: From<CosmicStackRenderElement<R>>,
    {
        let p = self.p();
        let windows = p.windows.lock().unwrap();
        let active = p.active.load(Ordering::SeqCst);
        let activated = p.activated.load(Ordering::Acquire);
        let appearance = p.appearance_conf.lock().unwrap();
        let tiled = p.tiled.load(Ordering::Acquire);

        if windows[active].is_maximized(false) {
            return None;
        }

        let round = appearance.clip_tiled_windows || !tiled;
        if tiled && !appearance.shadow_tiled_windows {
            return None;
        }
        let lt = crate::theme::lunaris_theme();
        let radii = if round {
            lt.radius_s
                .map(|x| if x < 4.0 { x } else { x + 4.0 })
                .map(|x| (x * scale as f32).round() as u8)
        } else {
            [0, 0, 0, 0]
        };

        let mut geo = SpaceElement::geometry(&windows[active]).to_f64();
        geo.size.h += TAB_HEIGHT as f64;
        if let Some(max_size) = max_size {
            geo.size = geo.size.clamp(Size::default(), max_size.to_f64());
        }

        geo = geo.upscale(scale);
        geo.loc += location.to_f64().to_logical(output_scale);

        let window_key =
            CosmicMappedKey(CosmicMappedKeyInner::Stack(Arc::downgrade(&self.inner)));

        Some(
            CosmicStackRenderElement::Shadow(ShadowShader::element(
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
        C: From<CosmicStackRenderElement<R>>,
    {
        if !{
            let p = self.p();
            p.override_alive.load(Ordering::Acquire)
        } {
            return Vec::new();
        }

        let geometry = {
            let p = self.p();
            p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].geometry()
        }
        .to_physical_precise_round(scale);
        let _stack_loc = location + geometry.loc;
        let window_loc = location + Point::from((0, (TAB_HEIGHT as f64 * scale.y) as i32));

        // No tab bar rendering from IcedElement; tab bar is rendered by desktop-shell
        // via the shell overlay protocol.
        let mut elements = Vec::new();

        elements.extend({
            let p = self.p();
            let windows = p.windows.lock().unwrap();
            let active = p.active.load(Ordering::SeqCst);
            let appearance = p.appearance_conf.lock().unwrap();
            let tiled = p.tiled.load(Ordering::Acquire);
            let maximized = windows[active].is_maximized(false);

            let lt = crate::theme::lunaris_theme();
            let round = (appearance.clip_tiled_windows || !tiled) && !maximized;
            let radii = round.then(|| {
                lt.radius_s
                    .map(|x| if x < 4.0 { x } else { x + 4.0 })
                    .map(|x| x.round() as u8)
            });

            let mut geo = SpaceElement::geometry(&windows[active]).to_f64();
            geo.loc += location.to_f64().to_logical(scale);
            geo.size.h += TAB_HEIGHT as f64;
            if let Some(max_size) = max_size {
                geo.size = geo.size.clamp(Size::default(), max_size.to_f64());
            }

            let window_key =
                CosmicMappedKey(CosmicMappedKeyInner::Stack(Arc::downgrade(&self.inner)));

            // Stack border removed: desktop-shell handles all window chrome
            // via the shell overlay protocol.
            std::iter::empty().chain(
                windows[active]
                    .render_elements::<R, WaylandSurfaceRenderElement<R>>(
                        renderer,
                        window_loc,
                        scale,
                        alpha,
                        scanout_override,
                    )
                    .into_iter()
                    .map(move |elem| {
                        let radii = radii.map(|[a, _, c, _]| [a, 0, c, 0]);
                        if radii.is_some_and(|radii| {
                            ClippedSurfaceRenderElement::will_clip(&elem, scale, geo, radii)
                        }) {
                            CosmicStackRenderElement::Clipped(ClippedSurfaceRenderElement::new(
                                renderer,
                                elem,
                                scale,
                                geo,
                                radii.unwrap(),
                            ))
                        } else {
                            CosmicStackRenderElement::Window(elem)
                        }
                    }),
            )
        });

        elements.into_iter().map(C::from).collect()
    }

    pub fn update_appearance_conf(&self, appearance: &AppearanceConfig) {
        let p = self.p();
        let mut conf = p.appearance_conf.lock().unwrap();
        if &*conf != appearance {
            *conf = *appearance;
        }
    }

    pub(crate) fn force_redraw(&self) {
        // No-op: tab bar rendering is handled by desktop-shell via the shell overlay protocol.
    }

    fn start_drag(&self, data: &mut State, seat: &Seat<State>, serial: Serial) {
        if let Some(dragged_out) = {
            let p = self.p();
            p.potential_drag.lock().unwrap().take()
        }
            && let Some(surface) = {
                let p = self.p();
                p.windows.lock().unwrap().get(dragged_out).cloned()
            }
        {
            let seat = seat.clone();
            surface.try_force_undecorated(false);
            surface.send_configure();
            if let Some(surface) = surface.wl_surface().map(Cow::into_owned) {
                let _ = data.common.event_loop_handle.insert_idle(move |state| {
                    let res = state.common.shell.write().move_request(
                        &surface,
                        &seat,
                        serial,
                        ReleaseMode::NoMouseButtons,
                        true,
                        &state.common.config,
                        &state.common.event_loop_handle,
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
        }
    }

    pub fn min_size(&self) -> Option<Size<i32, Logical>> {
        self.surfaces()
            .fold(None, |min_size, window| {
                let win_min_size = window.min_size_without_ssd();
                match (min_size, win_min_size) {
                    (None, None) => None,
                    (None, x) | (x, None) => x,
                    (Some(min1), Some(min2)) => {
                        Some((min1.w.max(min2.w), min1.h.max(min2.h)).into())
                    }
                }
            })
            .map(|size| size + (0, TAB_HEIGHT).into())
    }
    pub fn max_size(&self) -> Option<Size<i32, Logical>> {
        let theoretical_max = self
            .surfaces()
            .fold(None, |max_size, window| {
                let win_max_size = window.max_size_without_ssd();
                match (max_size, win_max_size) {
                    (None, None) => None,
                    (None, x) | (x, None) => x,
                    (Some(max1), Some(max2)) => Some(
                        (
                            if max1.w == 0 {
                                max2.w
                            } else if max2.w == 0 {
                                max1.w
                            } else {
                                max1.w.min(max2.w)
                            },
                            if max1.h == 0 {
                                max2.h
                            } else if max2.h == 0 {
                                max1.h
                            } else {
                                max1.h.min(max2.h)
                            },
                        )
                            .into(),
                    ),
                }
            })
            .map(|size| size + (0, TAB_HEIGHT).into());
        // The problem is, with accumulated sizes, the minimum size could be larger than our maximum...
        let min_size = self.min_size();
        match (theoretical_max, min_size) {
            (None, _) => None,
            (Some(max), None) => Some(max),
            (Some(max), Some(min)) => Some((max.w.max(min.w), max.h.max(min.h)).into()),
        }
    }

    pub fn corner_radius(&self, geometry_size: Size<i32, Logical>, _default_radius: u8) -> [u8; 4] {
        let p = self.p();
        let active_window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
        let is_tiled = p.tiled.load(Ordering::Acquire);
        let appearance = p.appearance_conf.lock().unwrap();
        let maximized = active_window.is_maximized(false);

        let round = (appearance.clip_tiled_windows || !is_tiled) && !maximized;
        let radii = crate::theme::lunaris_theme()
            .radius_s
            .map(|x| if x < 4.0 { x } else { x + 4.0 })
            .map(|val| val.round() as u8);

        if !round {
            let mut corners = active_window
                .corner_radius(geometry_size)
                .unwrap_or([_default_radius; 4]);

            corners[1] = 0;
            corners[3] = 0;

            corners
        } else {
            let mut corners = active_window.corner_radius(geometry_size).unwrap_or(radii);

            corners[0] = radii[0].max(corners[0]);
            corners[1] = radii[1];
            corners[2] = radii[2].max(corners[2]);
            corners[3] = radii[3];

            corners
        }
    }
}

impl IsAlive for CosmicStack {
    fn alive(&self) -> bool {
        let p = self.p();
        p.override_alive.load(Ordering::SeqCst)
            && p.windows.lock().unwrap().iter().any(IsAlive::alive)
    }
}

impl SpaceElement for CosmicStack {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        let p = self.p();
        let mut bbox =
            SpaceElement::bbox(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]);
        bbox.loc -= Point::from((RESIZE_BORDER, RESIZE_BORDER));
        bbox.size += Size::from((RESIZE_BORDER * 2, RESIZE_BORDER * 2));
        bbox.size.h += TAB_HEIGHT;
        bbox
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        self.focus_under(*point, WindowSurfaceType::ALL).is_some()
    }
    fn set_activate(&self, activated: bool) {
        let changed = {
            let p = self.p();
            if !p.group_focused.load(Ordering::SeqCst) {
                p.windows
                    .lock()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .for_each(|(i, w)| {
                        w.set_activated(activated && p.active.load(Ordering::SeqCst) == i);
                        w.send_configure();
                    });
            }
            p.activated.swap(activated, Ordering::SeqCst) != activated
        };

        let _ = changed;
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        let p = self.p();
        p.windows
            .lock()
            .unwrap()
            .iter()
            .for_each(|w| SpaceElement::output_enter(w, output, overlap))
    }
    fn output_leave(&self, output: &Output) {
        let p = self.p();
        p.windows
            .lock()
            .unwrap()
            .iter()
            .for_each(|w| SpaceElement::output_leave(w, output))
    }
    fn geometry(&self) -> Rectangle<i32, Logical> {
        let p = self.p();
        let mut geo =
            SpaceElement::geometry(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]);
        geo.size.h += TAB_HEIGHT;
        geo
    }
    fn z_index(&self) -> u8 {
        let p = self.p();
        SpaceElement::z_index(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)])
    }
    fn refresh(&self) {
        let p = self.p();
        let mut windows = p.windows.lock().unwrap();

        // don't let the stack become empty
        let old_active = p.active.load(Ordering::SeqCst);
        let active = windows[old_active].clone();
        windows.retain(IsAlive::alive);
        if windows.is_empty() {
            windows.push(active);
        }

        let len = windows.len();
        let _ = p
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                (active >= len).then_some(len - 1)
            });
        let active = p.active.load(Ordering::SeqCst);

        windows.iter().enumerate().for_each(|(i, w)| {
            if i == active {
                w.set_suspended(false);
            } else {
                w.set_suspended(true);
            }
            w.send_configure();

            SpaceElement::refresh(w)
        });
    }
}

impl KeyboardTarget<State> for CosmicStack {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        let p = self.p();
        let active = p.active.load(Ordering::SeqCst);
        p.previous_keyboard.store(active, Ordering::SeqCst);
        KeyboardTarget::enter(
            &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
            seat,
            data,
            keys,
            serial,
        )
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        let p = self.p();
        p.group_focused.store(false, Ordering::SeqCst);
        KeyboardTarget::leave(&p.windows.lock().unwrap()[active], seat, data, serial)
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
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        let p = self.p();
        if !p.group_focused.load(Ordering::SeqCst) {
            KeyboardTarget::key(
                &p.windows.lock().unwrap()[active],
                seat,
                data,
                key,
                state,
                serial,
                time,
            )
        }
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        let p = self.p();
        if !p.group_focused.load(Ordering::SeqCst) {
            KeyboardTarget::modifiers(
                &p.windows.lock().unwrap()[active],
                seat,
                data,
                modifiers,
                serial,
            )
        }
    }
}

impl PointerTarget<State> for CosmicStack {
    fn enter(&self, seat: &Seat<State>, _data: &mut State, event: &MotionEvent) {
        let p = self.p();
        let active_window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
        let Some(next) = Focus::under(active_window, TAB_HEIGHT, event.location) else {
            return;
        };
        let _old_focus = p.swap_focus(Some(next));

        let mut cursor_state = seat
            .user_data()
            .get::<CursorState>()
            .unwrap()
            .lock()
            .unwrap();
        cursor_state.set_shape(next.cursor_shape());
        seat.set_cursor_image_status(CursorImageStatus::default_named());
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        let event = event.clone();
        {
            let p = self.p();
            let active = p.active.load(Ordering::SeqCst);
            let active_window = &p.windows.lock().unwrap()[active];
            let Some(next) = Focus::under(active_window, TAB_HEIGHT, event.location) else {
                return;
            };
            let _previous = p.swap_focus(Some(next));

            let mut cursor_state = seat
                .user_data()
                .get::<CursorState>()
                .unwrap()
                .lock()
                .unwrap();
            cursor_state.set_shape(next.cursor_shape());
            seat.set_cursor_image_status(CursorImageStatus::default_named());
        }

        let active_window_geo = {
            let p = self.p();
            p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].geometry()
        };
        let adjusted_location = event.location - active_window_geo.loc.to_f64();

        if adjusted_location.y < 0.0
            || adjusted_location.y > TAB_HEIGHT as f64
            || adjusted_location.x < 64.0
            || adjusted_location.x > (active_window_geo.size.w as f64 - 64.0)
        {
            self.start_drag(data, seat, event.serial);
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
        let current_focus = {
            let p = self.p();
            p.current_focus()
        };
        match current_focus {
            Some(Focus::Header) => {
                // Left click: start drag. Right click: open context menu.
                if event.state == smithay::backend::input::ButtonState::Pressed {
                    if event.button == 0x111 {
                        // Right click: open context menu
                        let seat = seat.clone();
                        let serial = event.serial;
                        let active_surface = {
                            let p = self.p();
                            let window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
                            window.wl_surface().map(Cow::into_owned)
                        };
                        if let Some(surface) = active_surface {
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

                                    let mut cursor = seat
                                        .get_pointer()
                                        .unwrap()
                                        .current_location()
                                        .to_i32_round();
                                    cursor.y -= TAB_HEIGHT;
                                    let res = shell.menu_request(
                                        false,
                                        &surface,
                                        &seat,
                                        serial,
                                        cursor - position.as_logical(),
                                        true,
                                        &state.common.config,
                                        &state.common.event_loop_handle,
                                        &state.common.display_handle,
                                        &mut state.common.shell_overlay_state,
                                        &mut state.common.pending_menu_callbacks,
                                    );

                                    std::mem::drop(shell);
                                    if let Some((grab, focus)) = res {
                                        seat.get_pointer()
                                            .unwrap()
                                            .set_grab(state, grab, serial, focus);
                                    }
                                }
                            });
                        }
                    } else if event.button == 0x110 {
                        // Left click: start drag
                        let seat = seat.clone();
                        let serial = event.serial;
                        let active_surface = {
                            let p = self.p();
                            let active = p.active.load(Ordering::SeqCst);
                            p.windows.lock().unwrap()[active]
                                .wl_surface()
                                .map(Cow::into_owned)
                        };
                        if let Some(surface) = active_surface {
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
                    }
                }
            }
            Some(x) => {
                let serial = event.serial;
                let seat = seat.clone();
                let Some(surface) = ({
                    let p = self.p();
                    let window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
                    window.wl_surface().map(Cow::into_owned)
                }) else {
                    return;
                };
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
        // No-op: tab bar scrolling was handled by IcedElement.
    }

    fn frame(&self, _seat: &Seat<State>, _data: &mut State) {
        // No-op.
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: u32) {
        {
            let p = self.p();
            let mut cursor_state = seat
                .user_data()
                .get::<CursorState>()
                .unwrap()
                .lock()
                .unwrap();
            cursor_state.unset_shape();
            let _previous = p.swap_focus(None);
        }

        if let Some(dragged_out) = {
            let p = self.p();
            p.potential_drag.lock().unwrap().take()
        }
            && let Some(surface) = {
                let p = self.p();
                p.windows.lock().unwrap().get(dragged_out).cloned()
            }
        {
            let seat = seat.clone();
            surface.try_force_undecorated(false);
            surface.send_configure();
            if let Some(surface) = surface.wl_surface().map(Cow::into_owned) {
                let _ = data.common.event_loop_handle.insert_idle(move |state| {
                    let res = state.common.shell.write().move_request(
                        &surface,
                        &seat,
                        serial,
                        ReleaseMode::NoMouseButtons,
                        true,
                        &state.common.config,
                        &state.common.event_loop_handle,
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
        }

        let _ = (serial, time);
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

impl TouchTarget<State> for CosmicStack {
    fn down(&self, _seat: &Seat<State>, _data: &mut State, event: &DownEvent, _seq: Serial) {
        let _event = event.clone();
        let _active_window_geo = {
            let p = self.p();
            p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].geometry()
        };
        // Coordinate adjustment kept for future use; no IcedElement delegation.
    }

    fn up(&self, _seat: &Seat<State>, _data: &mut State, _event: &UpEvent, _seq: Serial) {
        // No-op.
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &TouchMotionEvent, seq: Serial) {
        let event = event.clone();
        let active_window_geo = {
            let p = self.p();
            p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].geometry()
        };
        let adjusted_location = event.location - active_window_geo.loc.to_f64();

        if adjusted_location.y < 0.0
            || adjusted_location.y > TAB_HEIGHT as f64
            || adjusted_location.x < 64.0
            || adjusted_location.x > (active_window_geo.size.w as f64 - 64.0)
        {
            self.start_drag(data, seat, seq);
        }
    }

    fn frame(&self, _seat: &Seat<State>, _data: &mut State, _seq: Serial) {
        // No-op.
    }

    fn cancel(&self, _seat: &Seat<State>, _data: &mut State, _seq: Serial) {
        // No-op.
    }

    fn shape(&self, _seat: &Seat<State>, _data: &mut State, _event: &ShapeEvent, _seq: Serial) {
        // No-op.
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

pub enum CosmicStackRenderElement<R: Renderer + ImportAll + ImportMem> {
    Header(MemoryRenderBufferRenderElement<R>),
    Shadow(PixelShaderElement),
    Border(PixelShaderElement),
    Window(WaylandSurfaceRenderElement<R>),
    Clipped(ClippedSurfaceRenderElement<R>),
}

impl<R: Renderer + ImportAll + ImportMem> From<MemoryRenderBufferRenderElement<R>>
    for CosmicStackRenderElement<R>
{
    fn from(value: MemoryRenderBufferRenderElement<R>) -> Self {
        Self::Header(value)
    }
}

impl<R: Renderer + ImportAll + ImportMem> From<WaylandSurfaceRenderElement<R>>
    for CosmicStackRenderElement<R>
{
    fn from(value: WaylandSurfaceRenderElement<R>) -> Self {
        Self::Window(value)
    }
}

impl<R: Renderer + ImportAll + ImportMem> From<ClippedSurfaceRenderElement<R>>
    for CosmicStackRenderElement<R>
{
    fn from(value: ClippedSurfaceRenderElement<R>) -> Self {
        Self::Clipped(value)
    }
}

impl<R> Element for CosmicStackRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem,
{
    fn id(&self) -> &RendererId {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.id(),
            CosmicStackRenderElement::Shadow(elem) => elem.id(),
            CosmicStackRenderElement::Border(elem) => elem.id(),
            CosmicStackRenderElement::Window(elem) => elem.id(),
            CosmicStackRenderElement::Clipped(elem) => elem.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.current_commit(),
            CosmicStackRenderElement::Shadow(elem) => elem.current_commit(),
            CosmicStackRenderElement::Border(elem) => elem.current_commit(),
            CosmicStackRenderElement::Window(elem) => elem.current_commit(),
            CosmicStackRenderElement::Clipped(elem) => elem.current_commit(),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.src(),
            CosmicStackRenderElement::Shadow(elem) => elem.src(),
            CosmicStackRenderElement::Border(elem) => elem.src(),
            CosmicStackRenderElement::Window(elem) => elem.src(),
            CosmicStackRenderElement::Clipped(elem) => elem.src(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.geometry(scale),
            CosmicStackRenderElement::Shadow(elem) => elem.geometry(scale),
            CosmicStackRenderElement::Border(elem) => elem.geometry(scale),
            CosmicStackRenderElement::Window(elem) => elem.geometry(scale),
            CosmicStackRenderElement::Clipped(elem) => elem.geometry(scale),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.location(scale),
            CosmicStackRenderElement::Shadow(elem) => elem.location(scale),
            CosmicStackRenderElement::Border(elem) => elem.location(scale),
            CosmicStackRenderElement::Window(elem) => elem.location(scale),
            CosmicStackRenderElement::Clipped(elem) => elem.location(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.transform(),
            CosmicStackRenderElement::Shadow(elem) => elem.transform(),
            CosmicStackRenderElement::Border(elem) => elem.transform(),
            CosmicStackRenderElement::Window(elem) => elem.transform(),
            CosmicStackRenderElement::Clipped(elem) => elem.transform(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.damage_since(scale, commit),
            CosmicStackRenderElement::Shadow(elem) => elem.damage_since(scale, commit),
            CosmicStackRenderElement::Border(elem) => elem.damage_since(scale, commit),
            CosmicStackRenderElement::Window(elem) => elem.damage_since(scale, commit),
            CosmicStackRenderElement::Clipped(elem) => elem.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.opaque_regions(scale),
            CosmicStackRenderElement::Shadow(elem) => elem.opaque_regions(scale),
            CosmicStackRenderElement::Border(elem) => elem.opaque_regions(scale),
            CosmicStackRenderElement::Window(elem) => elem.opaque_regions(scale),
            CosmicStackRenderElement::Clipped(elem) => elem.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.alpha(),
            CosmicStackRenderElement::Shadow(elem) => elem.alpha(),
            CosmicStackRenderElement::Border(elem) => elem.alpha(),
            CosmicStackRenderElement::Window(elem) => elem.alpha(),
            CosmicStackRenderElement::Clipped(elem) => elem.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.kind(),
            CosmicStackRenderElement::Shadow(elem) => elem.kind(),
            CosmicStackRenderElement::Border(elem) => elem.kind(),
            CosmicStackRenderElement::Window(elem) => elem.kind(),
            CosmicStackRenderElement::Clipped(elem) => elem.kind(),
        }
    }
}

impl<R> RenderElement<R> for CosmicStackRenderElement<R>
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
            CosmicStackRenderElement::Header(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            CosmicStackRenderElement::Shadow(elem) | CosmicStackRenderElement::Border(elem) => {
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
            CosmicStackRenderElement::Window(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            CosmicStackRenderElement::Clipped(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
        }
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        match self {
            CosmicStackRenderElement::Header(elem) => elem.underlying_storage(renderer),
            CosmicStackRenderElement::Shadow(elem) | CosmicStackRenderElement::Border(elem) => {
                elem.underlying_storage(renderer.glow_renderer_mut())
            }
            CosmicStackRenderElement::Window(elem) => elem.underlying_storage(renderer),
            CosmicStackRenderElement::Clipped(elem) => elem.underlying_storage(renderer),
        }
    }
}
