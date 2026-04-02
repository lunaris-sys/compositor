use std::{
    fmt,
    sync::{Arc, Mutex},
};

use calloop::LoopHandle;
use smithay::{
    backend::renderer::{
        ImportMem, Renderer,
        element::memory::MemoryRenderBufferRenderElement,
    },
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            GrabStartData as PointerGrabStartData, MotionEvent as PointerMotionEvent, PointerGrab,
            PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{
            GrabStartData as TouchGrabStartData,
            TouchGrab, TouchInnerHandle,
        },
    },
    output::Output,
    utils::{Logical, Point, Size},
};

use crate::{
    shell::focus::target::PointerFocusTarget,
    state::State,
    utils::prelude::*,
    wayland::protocols::shell_overlay::WindowAction,
};

use super::{GrabStartData, ResizeEdge};

mod default;
pub use self::default::*;

/// Persistent state for an active menu grab, stored on the seat.
pub struct MenuGrabState {
    screen_space_relative: Option<Output>,
    /// Set when the overlay protocol is active for this grab.
    /// Rendering is always delegated to desktop-shell.
    pub menu_id: Option<u32>,
}
pub type SeatMenuGrabState = Mutex<Option<MenuGrabState>>;

impl MenuGrabState {
    /// Render elements for the menu.
    ///
    /// With the overlay protocol active, rendering is handled entirely by
    /// desktop-shell, so this always returns an empty list.
    pub fn render<I, R>(&self, _renderer: &mut R, _output: &Output) -> Vec<I>
    where
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
        I: From<MemoryRenderBufferRenderElement<R>>,
    {
        Vec::new()
    }

    /// Whether the menu is positioned in screen space.
    pub fn is_in_screen_space(&self) -> bool {
        self.screen_space_relative.is_some()
    }
}

#[derive(Clone)]
pub enum Item {
    Separator,
    Submenu {
        title: String,
        items: Vec<Item>,
    },
    Entry {
        title: String,
        shortcut: Option<String>,
        on_press: Arc<Box<dyn Fn(&LoopHandle<'_, State>) + Send + Sync>>,
        toggled: bool,
        submenu: bool,
        disabled: bool,
        /// The window management action this entry maps to in the overlay protocol.
        /// `None` for items that are not sent over the protocol (e.g. zoom menu entries).
        action: Option<WindowAction>,
    },
}

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Separator => write!(f, "Separator"),
            Self::Submenu { title, items } => f
                .debug_struct("Submenu")
                .field("title", title)
                .field("items", items)
                .finish(),
            Self::Entry {
                title,
                shortcut,
                on_press: _,
                toggled,
                submenu,
                disabled,
                action,
            } => f
                .debug_struct("Entry")
                .field("title", title)
                .field("shortcut", shortcut)
                .field("on_press", &"...")
                .field("toggled", toggled)
                .field("submenu", submenu)
                .field("disabled", disabled)
                .field("action", action)
                .finish(),
        }
    }
}

impl Item {
    pub fn new<S: Into<String>, F: Fn(&LoopHandle<'_, State>) + Send + Sync + 'static>(
        title: S,
        on_press: F,
    ) -> Item {
        Item::Entry {
            title: title.into(),
            shortcut: None,
            on_press: Arc::new(Box::new(on_press)),
            toggled: false,
            submenu: false,
            disabled: false,
            action: None,
        }
    }

    /// Set the `WindowAction` this entry maps to in the overlay protocol.
    pub fn action(mut self, action: WindowAction) -> Self {
        if let Item::Entry {
            action: ref mut a, ..
        } = self
        {
            *a = Some(action);
        }
        self
    }

    pub fn new_submenu<S: Into<String>>(title: S, items: Vec<Item>) -> Item {
        Item::Submenu {
            title: title.into(),
            items,
        }
    }

    pub fn shortcut(mut self, shortcut: impl Into<Option<String>>) -> Self {
        if let Item::Entry {
            shortcut: ref mut s,
            ..
        } = self
        {
            *s = shortcut.into();
        }
        self
    }

    pub fn toggled(mut self, toggled: bool) -> Self {
        if let Item::Entry {
            toggled: ref mut t, ..
        } = self
        {
            *t = toggled;
        }
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        if let Item::Entry {
            disabled: ref mut d,
            ..
        } = self
        {
            *d = disabled;
        }
        self
    }
}

/// Active menu grab.
///
/// The menu is always rendered by desktop-shell via the `lunaris-shell-overlay`
/// protocol. Pointer events are forwarded to `shell_focus` so that the
/// desktop-shell client can detect clicks on the rendered menu.
pub struct MenuGrab {
    start_data: GrabStartData,
    seat: Seat<State>,
    /// Desktop-shell surface focus target for pointer event routing.
    shell_focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
}

impl PointerGrab<State> for MenuGrab {
    fn motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &PointerMotionEvent,
    ) {
        // Forward pointer events to desktop-shell so it can handle menu interaction.
        handle.motion(state, self.shell_focus.clone(), event);
    }

    fn relative_motion(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.relative_motion(state, None, event);
    }

    fn button(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        // If no shell client is connected, the grab has no way to be released
        // via protocol (no activate/dismiss will arrive). Release immediately
        // on any button press to prevent the pointer from getting stuck.
        if self.shell_focus.is_none() {
            handle.unset_grab(self, state, event.serial, event.time, true);
            return;
        }
        // Forward button events to desktop-shell.
        // The grab is released when desktop-shell sends activate or dismiss.
        handle.button(state, event);
    }

    fn axis(
        &mut self,
        state: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(state, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data)
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        match &self.start_data {
            GrabStartData::Pointer(start_data) => start_data,
            _ => unreachable!(),
        }
    }

    fn unset(&mut self, _data: &mut State) {}
}

impl TouchGrab<State> for MenuGrab {
    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &smithay::input::touch::DownEvent,
        seq: smithay::utils::Serial,
    ) {
        handle.down(data, None, event, seq);
    }

    fn up(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _event: &smithay::input::touch::UpEvent,
        _seq: smithay::utils::Serial,
    ) {
        handle.unset_grab(self, data);
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &smithay::input::touch::MotionEvent,
        seq: smithay::utils::Serial,
    ) {
        handle.motion(data, None, event, seq);
    }

    fn frame(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        seq: smithay::utils::Serial,
    ) {
        handle.frame(data, seq);
    }

    fn cancel(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        seq: smithay::utils::Serial,
    ) {
        handle.cancel(data, seq);
    }

    fn shape(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &smithay::input::touch::ShapeEvent,
        seq: smithay::utils::Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &smithay::input::touch::OrientationEvent,
        seq: smithay::utils::Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &TouchGrabStartData<State> {
        match &self.start_data {
            GrabStartData::Touch(start_data) => start_data,
            _ => unreachable!(),
        }
    }

    fn unset(&mut self, _data: &mut State) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuAlignment {
    pub x: AxisAlignment,
    pub y: AxisAlignment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisAlignment {
    Corner(u32),
    Centered,
    PreferCentered,
}

impl MenuAlignment {
    pub const CORNER: Self = MenuAlignment {
        x: AxisAlignment::Corner(0),
        y: AxisAlignment::Corner(0),
    };
    pub const PREFER_CENTERED: Self = MenuAlignment {
        x: AxisAlignment::PreferCentered,
        y: AxisAlignment::PreferCentered,
    };
    pub const CENTERED: Self = MenuAlignment {
        x: AxisAlignment::Centered,
        y: AxisAlignment::Centered,
    };
    pub const HORIZONTALLY_CENTERED: Self = MenuAlignment {
        x: AxisAlignment::Centered,
        y: AxisAlignment::Corner(0),
    };
    pub const VERTICALLY_CENTERED: Self = MenuAlignment {
        x: AxisAlignment::Corner(0),
        y: AxisAlignment::Centered,
    };

    pub fn horizontally_centered(offset: u32, fixed: bool) -> MenuAlignment {
        MenuAlignment {
            x: if fixed {
                AxisAlignment::Centered
            } else {
                AxisAlignment::PreferCentered
            },
            y: AxisAlignment::Corner(offset),
        }
    }

    pub fn vertically_centered(offset: u32, fixed: bool) -> MenuAlignment {
        MenuAlignment {
            x: AxisAlignment::Corner(offset),
            y: if fixed {
                AxisAlignment::Centered
            } else {
                AxisAlignment::PreferCentered
            },
        }
    }

    #[allow(dead_code)]
    fn rectangles(
        &self,
        position: Point<i32, Global>,
        size: Size<i32, Global>,
    ) -> Vec<smithay::utils::Rectangle<i32, Global>> {
        fn for_alignment(
            position: Point<i32, Global>,
            size: Size<i32, Global>,
            x: AxisAlignment,
            y: AxisAlignment,
        ) -> Vec<smithay::utils::Rectangle<i32, Global>> {
            match (x, y) {
                (AxisAlignment::Corner(x_offset), AxisAlignment::Corner(y_offset)) => {
                    let offset = Point::from((x_offset as i32, y_offset as i32));
                    vec![
                        smithay::utils::Rectangle::new(position + offset, size), // normal
                        smithay::utils::Rectangle::new(
                            position - Point::from((size.w, 0))
                                + Point::from((-(x_offset as i32), y_offset as i32)),
                            size,
                        ), // flipped left
                        smithay::utils::Rectangle::new(
                            position
                                - Point::from((0, size.h))
                                - Point::from((x_offset as i32, -(y_offset as i32))),
                            size,
                        ), // flipped up
                        smithay::utils::Rectangle::new(position - size.to_point() - offset, size), // flipped left & up
                    ]
                }
                (AxisAlignment::Centered, AxisAlignment::Corner(offset)) => {
                    let x = position.x - ((size.w as f64 / 2.).round() as i32);
                    vec![
                        smithay::utils::Rectangle::new(
                            Point::from((x, position.y + offset as i32)),
                            size,
                        ), // below
                        smithay::utils::Rectangle::new(
                            Point::from((x, position.y - size.h - offset as i32)),
                            size,
                        ), // above
                    ]
                }
                (AxisAlignment::Corner(offset), AxisAlignment::Centered) => {
                    let y = position.y - ((size.h as f64 / 2.).round() as i32);
                    vec![
                        smithay::utils::Rectangle::new(
                            Point::from((position.x + offset as i32, y)),
                            size,
                        ), // left
                        smithay::utils::Rectangle::new(
                            Point::from((position.x - size.w - offset as i32, y)),
                            size,
                        ), // right
                    ]
                }
                (AxisAlignment::Centered, AxisAlignment::Centered) => {
                    vec![smithay::utils::Rectangle::new(
                        position - size.to_f64().downscale(2.).to_i32_round().to_point(),
                        size,
                    )]
                }
                (AxisAlignment::PreferCentered, AxisAlignment::PreferCentered) => for_alignment(
                    position,
                    size,
                    AxisAlignment::Centered,
                    AxisAlignment::Centered,
                )
                .into_iter()
                .chain(for_alignment(
                    position,
                    size,
                    AxisAlignment::Centered,
                    AxisAlignment::Corner(0),
                ))
                .chain(for_alignment(
                    position,
                    size,
                    AxisAlignment::Corner(0),
                    AxisAlignment::Centered,
                ))
                .chain(for_alignment(
                    position,
                    size,
                    AxisAlignment::Corner(0),
                    AxisAlignment::Corner(0),
                ))
                .collect(),
                (AxisAlignment::PreferCentered, y) => {
                    for_alignment(position, size, AxisAlignment::Centered, y)
                        .into_iter()
                        .chain(for_alignment(position, size, AxisAlignment::Corner(0), y))
                        .collect()
                }
                (x, AxisAlignment::PreferCentered) => {
                    for_alignment(position, size, x, AxisAlignment::Centered)
                        .into_iter()
                        .chain(for_alignment(position, size, x, AxisAlignment::Corner(0)))
                        .collect()
                }
            }
        }

        for_alignment(position, size, self.x, self.y)
    }
}

impl MenuGrab {
    /// Create a new `MenuGrab`.
    ///
    /// `menu_id` identifies the overlay protocol menu. Rendering and interaction
    /// are handled by desktop-shell; pointer events are forwarded to `shell_focus`.
    pub fn new(
        start_data: GrabStartData,
        seat: &Seat<State>,
        _items: impl Iterator<Item = Item>,
        _position: Point<i32, Global>,
        _alignment: MenuAlignment,
        screen_space_relative: Option<f64>,
        _handle: LoopHandle<'static, crate::state::State>,
        menu_id: Option<u32>,
        shell_focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
    ) -> MenuGrab {
        let output = seat.active_output();
        let screen_space_output = screen_space_relative.is_some().then_some(output.clone());

        let grab_state = MenuGrabState {
            screen_space_relative: screen_space_output,
            menu_id,
        };
        *seat
            .user_data()
            .get::<SeatMenuGrabState>()
            .unwrap()
            .lock()
            .unwrap() = Some(grab_state);

        MenuGrab {
            start_data,
            seat: seat.clone(),
            shell_focus,
        }
    }

    /// Whether this grab was initiated by a touch event.
    pub fn is_touch_grab(&self) -> bool {
        match self.start_data {
            GrabStartData::Touch(_) => true,
            GrabStartData::Pointer(_) => false,
        }
    }
}

impl Drop for MenuGrab {
    fn drop(&mut self) {
        self.seat
            .user_data()
            .get::<SeatMenuGrabState>()
            .unwrap()
            .lock()
            .unwrap()
            .take();
        // NOTE: `context_menu_closed` (compositor-initiated close) is not sent
        // from Drop because `LoopHandle` is not `Send` and cannot be stored here.
        // Explicit compositor-side teardown (e.g. window destroyed while menu is
        // open) must call `ShellOverlayState::close_context_menu` at the call site.
    }
}
