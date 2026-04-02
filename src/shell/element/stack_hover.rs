use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

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

/// Lightweight indicator element shown when a window can be stacked.
///
/// Previously backed by `IcedElement`; now a standalone struct. Rendering
/// is handled by `desktop-shell` via `lunaris-shell-overlay`, so all
/// `render_elements` calls return an empty `Vec`.
#[derive(Clone)]
pub struct StackHover {
    inner: Arc<Mutex<StackHoverInternal>>,
}

struct StackHoverInternal {
    size: Size<i32, Logical>,
}

impl fmt::Debug for StackHover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackHover").finish_non_exhaustive()
    }
}

impl PartialEq for StackHover {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for StackHover {}

impl Hash for StackHover {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.inner) as usize).hash(state)
    }
}

/// Creates a new [`StackHover`] indicator with the given size.
pub fn stack_hover(
    _evlh: calloop::LoopHandle<'static, crate::state::State>,
    size: Size<i32, Logical>,
    _theme: cosmic::Theme,
) -> StackHover {
    StackHover {
        inner: Arc::new(Mutex::new(StackHoverInternal { size })),
    }
}

impl StackHover {
    /// Updates the indicator's logical size.
    pub fn resize(&self, size: Size<i32, Logical>) {
        self.inner.lock().unwrap().size = size;
    }

    /// No-op -- output tracking is unused.
    pub fn output_enter(&self, _output: &Output, _overlap: Rectangle<i32, Logical>) {}

    /// No-op -- output tracking is unused.
    pub fn output_leave(&self, _output: &Output) {}

    /// Returns the current logical size.
    pub fn current_size(&self) -> Size<i32, Logical> {
        self.inner.lock().unwrap().size
    }
}

impl<R> AsRenderElements<R> for StackHover
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

impl IsAlive for StackHover {
    fn alive(&self) -> bool {
        true
    }
}

impl SpaceElement for StackHover {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        Rectangle::from_size(self.inner.lock().unwrap().size)
    }

    fn is_in_input_region(&self, _point: &Point<f64, Logical>) -> bool {
        false
    }

    fn set_activate(&self, _activated: bool) {}

    fn output_enter(&self, _output: &Output, _overlap: Rectangle<i32, Logical>) {}

    fn output_leave(&self, _output: &Output) {}
}

impl PointerTarget<crate::state::State> for StackHover {
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

impl TouchTarget<crate::state::State> for StackHover {
    fn down(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &DownEvent, _seq: Serial) {}
    fn up(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &UpEvent, _seq: Serial) {}
    fn motion(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &TouchMotionEvent, _seq: Serial) {}
    fn frame(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _seq: Serial) {}
    fn cancel(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _seq: Serial) {}
    fn shape(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &ShapeEvent, _seq: Serial) {}
    fn orientation(&self, _seat: &Seat<crate::state::State>, _data: &mut crate::state::State, _event: &OrientationEvent, _seq: Serial) {}
}
