use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use calloop::LoopHandle;
use cosmic_settings_config::shortcuts::action::ResizeDirection;
use smithay::{
    backend::renderer::{ImportMem, Renderer, element::memory::MemoryRenderBufferRenderElement},
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
        },
        touch::{DownEvent, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchTarget, UpEvent},
    },
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size},
};

use smithay::backend::renderer::element::AsRenderElements;
use smithay::desktop::space::SpaceElement;

use crate::shell::grabs::ResizeEdge;

// SAFETY: LoopHandle contains Rc which is !Send, but ResizeIndicator is never
// moved across threads or dropped on a foreign thread. Same safety contract as
// the former IcedElement wrapper.
unsafe impl Send for ResizeIndicator {}
unsafe impl Sync for ResizeIndicator {}

/// Lightweight indicator element for resize operations.
///
/// Previously backed by `IcedElement`; now a standalone struct that stores
/// resize metadata and exposes the same public API surface. Rendering is
/// handled by `desktop-shell` via `lunaris-shell-overlay`, so all
/// `render_elements` calls return an empty `Vec`.
#[derive(Clone)]
pub struct ResizeIndicator {
    inner: Arc<Mutex<ResizeIndicatorInternal>>,
    handle: LoopHandle<'static, crate::state::State>,
}

impl fmt::Debug for ResizeIndicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResizeIndicator").finish_non_exhaustive()
    }
}

impl PartialEq for ResizeIndicator {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ResizeIndicator {}

impl Hash for ResizeIndicator {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.inner) as usize).hash(state)
    }
}

/// Internal data for [`ResizeIndicator`].
pub struct ResizeIndicatorInternal {
    /// Current active resize edges.
    pub edges: Mutex<ResizeEdge>,
    /// Resize direction (outwards or inwards).
    pub direction: ResizeDirection,
    /// Formatted shortcut string for the outwards action.
    pub shortcut1: String,
    /// Formatted shortcut string for the inwards action.
    pub shortcut2: String,
}

/// Creates a new [`ResizeIndicator`] pre-populated with keybinding info.
pub fn resize_indicator(
    direction: ResizeDirection,
    config: &crate::config::Config,
    evlh: LoopHandle<'static, crate::state::State>,
    _theme: cosmic::Theme,
) -> ResizeIndicator {
    use cosmic_settings_config::shortcuts::action::Action;
    ResizeIndicator {
        inner: Arc::new(Mutex::new(ResizeIndicatorInternal {
            edges: Mutex::new(ResizeEdge::all()),
            direction,
            shortcut1: config
                .shortcuts
                .iter()
                .find_map(|(pattern, action)| {
                    (*action == Action::Resizing(ResizeDirection::Outwards))
                        .then_some(format!("{}: ", pattern.to_string()))
                })
                .unwrap_or_else(|| crate::fl!("unknown-keybinding")),
            shortcut2: config
                .shortcuts
                .iter()
                .find_map(|(pattern, action)| {
                    (*action == Action::Resizing(ResizeDirection::Inwards))
                        .then_some(format!("{}: ", pattern.to_string()))
                })
                .unwrap_or_else(|| crate::fl!("unknown-keybinding")),
        })),
        handle: evlh,
    }
}

impl ResizeIndicator {
    /// Runs a closure against the internal state, matching the former
    /// `IcedElement::with_program` API.
    pub fn with_program<R>(&self, f: impl FnOnce(&ResizeIndicatorInternal) -> R) -> R {
        let internal = self.inner.lock().unwrap();
        f(&internal)
    }

    /// Returns the calloop `LoopHandle` stored at construction time.
    pub fn loop_handle(&self) -> LoopHandle<'static, crate::state::State> {
        self.handle.clone()
    }

    /// No-op -- previously triggered an Iced re-render.
    pub fn force_update(&self) {}

    /// No-op -- size is unused since desktop-shell handles rendering.
    pub fn resize(&self, _size: Size<i32, Logical>) {}

    /// No-op -- output tracking is unused.
    pub fn output_enter(&self, _output: &Output, _overlap: Rectangle<i32, Logical>) {}

    /// No-op -- output tracking is unused.
    pub fn output_leave(&self, _output: &Output) {}

    /// Returns an empty `Size` -- the indicator has no compositor-side geometry.
    pub fn current_size(&self) -> Size<i32, Logical> {
        Size::default()
    }
}

impl<R> AsRenderElements<R> for ResizeIndicator
where
    R: Renderer + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    type RenderElement = MemoryRenderBufferRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        _renderer: &mut R,
        _location: Point<i32, Physical>,
        _scale: Scale<f64>,
        _alpha: f32,
    ) -> Vec<C> {
        Vec::new()
    }
}

impl IsAlive for ResizeIndicator {
    fn alive(&self) -> bool {
        true
    }
}

impl SpaceElement for ResizeIndicator {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        Rectangle::default()
    }

    fn is_in_input_region(&self, _point: &Point<f64, Logical>) -> bool {
        false
    }

    fn set_activate(&self, _activated: bool) {}

    fn output_enter(&self, _output: &Output, _overlap: Rectangle<i32, Logical>) {}

    fn output_leave(&self, _output: &Output) {}
}

impl PointerTarget<crate::state::State> for ResizeIndicator {
    fn enter(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &MotionEvent) {}
    fn motion(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &MotionEvent) {}
    fn relative_motion(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &RelativeMotionEvent) {}
    fn button(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &ButtonEvent) {}
    fn axis(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: AxisFrame) {}
    fn frame(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State) {}
    fn leave(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _serial: Serial, _time: u32) {}
    fn gesture_swipe_begin(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GestureSwipeBeginEvent) {}
    fn gesture_swipe_update(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GestureSwipeUpdateEvent) {}
    fn gesture_swipe_end(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GestureSwipeEndEvent) {}
    fn gesture_pinch_begin(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GesturePinchBeginEvent) {}
    fn gesture_pinch_update(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GesturePinchUpdateEvent) {}
    fn gesture_pinch_end(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GesturePinchEndEvent) {}
    fn gesture_hold_begin(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GestureHoldBeginEvent) {}
    fn gesture_hold_end(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &GestureHoldEndEvent) {}
}

impl TouchTarget<crate::state::State> for ResizeIndicator {
    fn down(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &DownEvent, _seq: Serial) {}
    fn up(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &UpEvent, _seq: Serial) {}
    fn motion(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &TouchMotionEvent, _seq: Serial) {}
    fn frame(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _seq: Serial) {}
    fn cancel(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _seq: Serial) {}
    fn shape(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &ShapeEvent, _seq: Serial) {}
    fn orientation(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &OrientationEvent, _seq: Serial) {}
}
