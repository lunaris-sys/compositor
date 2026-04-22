// SPDX-License-Identifier: GPL-3.0-only

use std::{any::Any, cell::RefCell, collections::HashMap, sync::Mutex};

use crate::{
    backend::render::cursor::CursorState,
    config::{Config, xkb_config_to_wl},
    input::{ModifiersShortcutQueue, SupressedButtons, SupressedKeys},
    state::State,
};
use smithay::{
    backend::input::{Device, DeviceCapability},
    desktop::utils::bbox_from_surface_tree,
    input::{
        Seat, SeatState,
        keyboard::{LedState, XkbConfig},
        pointer::{CursorImageAttributes, CursorImageStatus},
    },
    output::Output,
    reexports::{input::Device as InputDevice, wayland_server::DisplayHandle},
    utils::{Buffer, IsAlive, Monotonic, Point, Rectangle, Serial, Time, Transform},
    wayland::compositor::with_states,
};
use tracing::warn;

use super::grabs::{SeatMenuGrabState, SeatMoveGrabState};

crate::utils::id_gen!(next_seat_id, SEAT_ID, SEAT_IDS);

// for more information on seats, see:
// <https://wayland-book.com/print.html#seats-handling-input>
/// Seats are an abstraction over a set of input devices grouped together, such as a keyboard, pointer and touch device.
/// i.e. Those used by a user to operate the computer.
#[derive(Debug)]
pub struct Seats {
    seats: Vec<Seat<State>>,
    last_active: Option<Seat<State>>,
}

impl Default for Seats {
    fn default() -> Self {
        Self::new()
    }
}

impl Seats {
    pub fn new() -> Seats {
        Seats {
            seats: Vec::new(),
            last_active: None,
        }
    }

    pub fn add_seat(&mut self, seat: Seat<State>) {
        if self.seats.is_empty() {
            self.last_active = Some(seat.clone());
        }
        self.seats.push(seat);
    }

    pub fn remove_seat(&mut self, seat: &Seat<State>) {
        self.seats.retain(|s| s != seat);
        if self.seats.is_empty() {
            self.last_active = None;
        } else if self.last_active.as_ref().is_some_and(|s| s == seat) {
            self.last_active = Some(self.seats[0].clone());
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Seat<State>> {
        self.seats.iter()
    }

    pub fn last_active(&self) -> &Seat<State> {
        self.last_active.as_ref().expect("No seat?")
    }

    pub fn update_last_active(&mut self, seat: &Seat<State>) {
        self.last_active = Some(seat.clone());
    }

    pub fn for_device<D: Device>(&self, device: &D) -> Option<&Seat<State>> {
        self.iter().find(|seat| {
            let userdata = seat.user_data();
            let devices = userdata.get::<Devices>().unwrap();
            devices.has_device(device)
        })
    }
}

impl Devices {
    pub fn add_device<D: Device + 'static>(
        &self,
        device: &D,
        led_state: LedState,
    ) -> Vec<DeviceCapability> {
        let id = device.id();
        let mut map = self.capabilities.borrow_mut();
        let caps = [
            DeviceCapability::Keyboard,
            DeviceCapability::Pointer,
            DeviceCapability::TabletTool,
        ]
        .iter()
        .cloned()
        .filter(|c| device.has_capability(*c))
        .collect::<Vec<_>>();
        let new_caps = caps
            .iter()
            .cloned()
            .filter(|c| map.values().flatten().all(|has| *c != *has))
            .collect::<Vec<_>>();
        map.insert(id, caps);

        if device.has_capability(DeviceCapability::Keyboard)
            && let Some(device) = <dyn Any>::downcast_ref::<InputDevice>(device)
        {
            let mut device = device.clone();
            device.led_update(led_state.into());
            self.keyboards.borrow_mut().push(device);
        }

        new_caps
    }

    pub fn has_device<D: Device>(&self, device: &D) -> bool {
        self.capabilities.borrow().contains_key(&device.id())
    }

    pub fn remove_device<D: Device>(&self, device: &D) -> Vec<DeviceCapability> {
        let id = device.id();

        let mut keyboards = self.keyboards.borrow_mut();
        if let Some(idx) = keyboards.iter().position(|x| x.id() == id) {
            keyboards.remove(idx);
        }

        let mut map = self.capabilities.borrow_mut();
        map.remove(&id)
            .unwrap_or_default()
            .into_iter()
            .filter(|c| map.values().flatten().all(|has| *c != *has))
            .collect()
    }

    pub fn update_led_state(&self, led_state: LedState) {
        for keyboard in self.keyboards.borrow_mut().iter_mut() {
            keyboard.led_update(led_state.into());
        }
    }
}

#[derive(Default)]
pub struct Devices {
    capabilities: RefCell<HashMap<String, Vec<DeviceCapability>>>,
    // Used for updating keyboard leds on kms backend
    keyboards: RefCell<Vec<InputDevice>>,
}

impl Default for SeatId {
    fn default() -> SeatId {
        SeatId(next_seat_id())
    }
}

impl Drop for SeatId {
    fn drop(&mut self) {
        SEAT_IDS.lock().unwrap().remove(&self.0);
    }
}

#[repr(transparent)]
struct SeatId(pub usize);

/// The output which contains the cursor associated with a seat.
struct ActiveOutput(pub Mutex<Output>);

/// The output which currently has keyboard focus
struct FocusedOutput(pub Mutex<Option<Output>>);

#[derive(Default)]
pub struct LastModifierChange(pub Mutex<Option<Serial>>);

/// Per-seat tracker for double-click recognition in SSD/stack title
/// regions. Populated by `CosmicWindow::button` and `CosmicStack::button`
/// on every press in a `Focus::Header` zone; invalidated on pointer
/// motion (any motion event seeds a baseline; motion beyond
/// `DCLK_DISTANCE_PX` between clicks disarms the tracker) and on
/// pointer-leave / focus-change out of the header zone.
///
/// A press is a double-click iff the previous press:
///  * was on the SAME surface,
///  * used the SAME mouse button, and
///  * happened within `DCLK_TIME_MS` milliseconds.
///
/// The "pointer didn't travel far between clicks" requirement is
/// enforced indirectly by `invalidate_on_motion`, which runs inside
/// `PointerTarget::motion` where the pointer coordinates are safely
/// available on the event. Deliberately NOT reading
/// `seat.get_pointer().current_location()` from inside
/// `PointerTarget::button` — Smithay holds the pointer's outer
/// `Mutex` for the full duration of button dispatch, so any
/// re-entrant `current_location()` call from that context deadlocks
/// the whole compositor (we hit exactly that bug on the first
/// press into a Kitty header).
///
/// Time comparisons use `wrapping_sub` because `ButtonEvent::time`
/// is `u32` milliseconds and wraps every ~49 days.
#[derive(Default)]
pub struct DoubleClickTracker {
    pub(crate) inner: Mutex<DoubleClickState>,
}

#[derive(Default, Debug, Clone)]
pub struct DoubleClickState {
    /// Event timestamp of the last recorded press (milliseconds,
    /// `ButtonEvent::time`). `None` = no baseline.
    pub last_time_ms: Option<u32>,
    /// Which mouse button was last pressed (0x110 = left, etc.).
    pub last_button: Option<u32>,
    /// Logical pointer location from the most recent motion event
    /// (populated by `invalidate_on_motion`, not by
    /// `observe_press` — see the type doc for why).
    pub last_motion_location: Option<Point<f64, smithay::utils::Logical>>,
    /// Opaque identifier of the surface the last click targeted.
    /// `wl_surface` protocol id for Wayland, `X11Window` XID for
    /// XWayland — only equality matters, not the bit layout.
    pub last_target_id: Option<u64>,
}

/// Maximum time between two presses that still counts as a
/// double-click. 300 ms is the common OS default (macOS/Windows).
pub const DCLK_TIME_MS: u32 = 300;

/// Maximum pointer travel between two consecutive motion events that
/// still keeps a double-click baseline armed. Anything larger is
/// treated as an intentional drag gesture and resets the tracker.
pub const DCLK_DISTANCE_PX: f64 = 10.0;

impl DoubleClickTracker {
    /// Record a fresh press and report whether it completes a
    /// double-click against the previously recorded press.
    ///
    /// After this call the tracker always holds `(time, button,
    /// target)` of the most recent press, regardless of the
    /// returned `bool`. That makes triple-clicks behave like
    /// single-click-then-double-click rather than double-click-twice.
    pub fn observe_press(&self, time_ms: u32, button: u32, target_id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let is_double = match (inner.last_time_ms, inner.last_button, inner.last_target_id) {
            (Some(prev_t), Some(prev_b), Some(prev_id)) => {
                let dt = time_ms.wrapping_sub(prev_t);
                prev_b == button && prev_id == target_id && dt <= DCLK_TIME_MS
            }
            _ => false,
        };
        if is_double {
            // Reset so a third click is treated as a fresh baseline.
            inner.last_time_ms = None;
            inner.last_button = None;
            inner.last_target_id = None;
        } else {
            inner.last_time_ms = Some(time_ms);
            inner.last_button = Some(button);
            inner.last_target_id = Some(target_id);
        }
        is_double
    }

    /// Invalidate the tracker. Call on pointer-leave, focus-change
    /// out of the header zone, etc.
    pub fn invalidate(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = DoubleClickState::default();
    }

    /// Update the motion baseline and invalidate if the pointer has
    /// travelled more than `DCLK_DISTANCE_PX` since the last motion
    /// sample. No-op when the tracker is empty. This runs inside
    /// `PointerTarget::motion` where the current pointer location
    /// is supplied on the event — **no** re-entrant
    /// `current_location()` call, so no lock-ordering hazard.
    pub fn invalidate_on_motion(&self, location: Point<f64, smithay::utils::Logical>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(prev_loc) = inner.last_motion_location {
            let dx = location.x - prev_loc.x;
            let dy = location.y - prev_loc.y;
            if (dx * dx + dy * dy).sqrt() > DCLK_DISTANCE_PX {
                let empty = DoubleClickState::default();
                *inner = empty;
            }
        }
        inner.last_motion_location = Some(location);
    }
}

#[cfg(test)]
mod dclk_tests {
    use super::*;
    use smithay::utils::Point;

    fn loc(x: f64, y: f64) -> Point<f64, smithay::utils::Logical> {
        Point::from((x, y))
    }

    #[test]
    fn first_press_is_not_a_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
    }

    #[test]
    fn second_press_within_threshold_is_a_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        assert!(t.observe_press(1200, 0x110, 1));
    }

    #[test]
    fn second_press_after_threshold_is_not_a_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        assert!(!t.observe_press(1000 + DCLK_TIME_MS + 1, 0x110, 1));
    }

    #[test]
    fn different_buttons_are_not_a_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        assert!(!t.observe_press(1100, 0x111, 1));
    }

    #[test]
    fn different_targets_are_not_a_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        assert!(!t.observe_press(1100, 0x110, 2));
    }

    #[test]
    fn motion_between_clicks_beyond_distance_disarms_double_click() {
        // Two motion samples separated by >DCLK_DISTANCE_PX disarm
        // the tracker. Second click is treated as a fresh baseline.
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        t.invalidate_on_motion(loc(100.0, 50.0));
        t.invalidate_on_motion(loc(200.0, 50.0));
        assert!(!t.observe_press(1100, 0x110, 1));
    }

    #[test]
    fn motion_between_clicks_within_distance_preserves_double_click() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        t.invalidate_on_motion(loc(100.0, 50.0));
        t.invalidate_on_motion(loc(103.0, 52.0));
        assert!(t.observe_press(1100, 0x110, 1));
    }

    #[test]
    fn triple_click_does_not_cascade() {
        // [single, double, single], not [single, double, double].
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        assert!(t.observe_press(1100, 0x110, 1));
        assert!(!t.observe_press(1200, 0x110, 1));
    }

    #[test]
    fn invalidate_clears_baseline() {
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        t.invalidate();
        assert!(!t.observe_press(1100, 0x110, 1));
    }

    #[test]
    fn time_wrap_is_tolerated() {
        let t = DoubleClickTracker::default();
        // last press happened right before u32::MAX wrap.
        let t0 = u32::MAX - 50;
        let t1 = 100u32; // Wrapped forward by 150 ms.
        assert!(!t.observe_press(t0, 0x110, 1));
        assert!(t.observe_press(t1, 0x110, 1));
    }

    #[test]
    fn first_motion_seeds_baseline_without_resetting() {
        // The very first motion event establishes the baseline and
        // must NOT invalidate an armed press — otherwise a user who
        // touches the mouse between two rapid clicks would lose
        // their double-click.
        let t = DoubleClickTracker::default();
        assert!(!t.observe_press(1000, 0x110, 1));
        t.invalidate_on_motion(loc(100.0, 50.0));
        assert!(t.observe_press(1100, 0x110, 1));
    }
}

pub fn create_seat(
    dh: &DisplayHandle,
    seat_state: &mut SeatState<State>,
    output: &Output,
    config: &Config,
    name: String,
) -> Seat<State> {
    let mut seat = seat_state.new_wl_seat(dh, name);
    let userdata = seat.user_data();
    userdata.insert_if_missing_threadsafe(SeatId::default);
    userdata.insert_if_missing(Devices::default);
    userdata.insert_if_missing(SupressedKeys::default);
    userdata.insert_if_missing(SupressedButtons::default);
    userdata.insert_if_missing(ModifiersShortcutQueue::default);
    userdata.insert_if_missing(LastModifierChange::default);
    userdata.insert_if_missing(DoubleClickTracker::default);
    userdata.insert_if_missing_threadsafe(SeatMoveGrabState::default);
    userdata.insert_if_missing_threadsafe(SeatMenuGrabState::default);
    userdata.insert_if_missing_threadsafe(CursorState::default);
    userdata.insert_if_missing_threadsafe(|| ActiveOutput(Mutex::new(output.clone())));
    userdata.insert_if_missing_threadsafe(|| FocusedOutput(Mutex::new(None)));
    userdata.insert_if_missing_threadsafe(|| Mutex::new(CursorImageStatus::default_named()));

    // A lot of clients bind keyboard and pointer unconditionally once on launch..
    // Initial clients might race the compositor on adding periheral and
    // end up in a state, where they are not able to receive input.
    // Additionally a lot of clients don't handle keyboards/pointer objects being
    // removed very well either and we don't want to crash applications, because the
    // user is replugging their keyboard or mouse.
    //
    // So instead of doing the right thing (and initialize these capabilities as matching
    // devices appear), we have to surrender to reality and just always expose a keyboard and pointer.
    let conf = config.xkb_config();
    tracing::info!(
        "seat keyboard init: layout={:?} variant={:?} model={:?} rules={:?} options={:?}",
        conf.layout, conf.variant, conf.model, conf.rules, conf.options,
    );
    seat.add_keyboard(
        xkb_config_to_wl(&conf),
        (conf.repeat_delay as i32).abs(),
        (conf.repeat_rate as i32).abs(),
    )
    .or_else(|err| {
        warn!(
            ?err,
            "Failed to load provided xkb config. Trying default...",
        );
        seat.add_keyboard(
            XkbConfig::default(),
            (conf.repeat_delay as i32).abs(),
            (conf.repeat_rate as i32).abs(),
        )
    })
    .expect("Failed to load xkb configuration files");
    seat.add_pointer();
    seat.add_touch();

    seat
}

pub trait SeatExt {
    fn id(&self) -> usize;

    fn active_output(&self) -> Output;
    fn focused_output(&self) -> Option<Output>;
    fn focused_or_active_output(&self) -> Output {
        self.focused_output()
            .unwrap_or_else(|| self.active_output())
    }
    fn set_active_output(&self, output: &Output);
    fn set_focused_output(&self, output: Option<&Output>);
    fn devices(&self) -> &Devices;
    fn supressed_keys(&self) -> &SupressedKeys;
    fn supressed_buttons(&self) -> &SupressedButtons;
    fn modifiers_shortcut_queue(&self) -> &ModifiersShortcutQueue;
    fn last_modifier_change(&self) -> Option<Serial>;
    fn double_click_tracker(&self) -> &DoubleClickTracker;

    fn cursor_geometry(
        &self,
        loc: impl Into<Point<f64, Buffer>>,
        time: Time<Monotonic>,
    ) -> Option<(Rectangle<i32, Buffer>, Point<i32, Buffer>)>;
    fn cursor_image_status(&self) -> CursorImageStatus;
    fn set_cursor_image_status(&self, status: CursorImageStatus);
}

impl SeatExt for Seat<State> {
    fn id(&self) -> usize {
        self.user_data().get::<SeatId>().unwrap().0
    }

    /// Returns the output that contains the cursor associated with a seat. Note that the window which has keyboard focus
    /// may be on a different output. Currently, to get the focused output, use [`Self::focused_output`].
    fn active_output(&self) -> Output {
        self.user_data()
            .get::<ActiveOutput>()
            .map(|x| x.0.lock().unwrap().clone())
            .unwrap()
    }

    /// Returns the output which currently has keyboard focus. If no window has keyboard focus (e.g. when there are no windows)
    /// the focused output will be the same as the active output.
    fn focused_output(&self) -> Option<Output> {
        if self
            .get_keyboard()
            .is_some_and(|k| k.current_focus().is_some())
        {
            self.user_data()
                .get::<FocusedOutput>()
                .map(|x| x.0.lock().unwrap().clone())?
        } else {
            None
        }
    }

    fn set_active_output(&self, output: &Output) {
        *self
            .user_data()
            .get::<ActiveOutput>()
            .unwrap()
            .0
            .lock()
            .unwrap() = output.clone();
    }

    fn set_focused_output(&self, output: Option<&Output>) {
        *self
            .user_data()
            .get::<FocusedOutput>()
            .unwrap()
            .0
            .lock()
            .unwrap() = output.cloned();
    }

    fn devices(&self) -> &Devices {
        self.user_data().get::<Devices>().unwrap()
    }

    fn supressed_keys(&self) -> &SupressedKeys {
        self.user_data().get::<SupressedKeys>().unwrap()
    }

    fn supressed_buttons(&self) -> &SupressedButtons {
        self.user_data().get::<SupressedButtons>().unwrap()
    }

    fn modifiers_shortcut_queue(&self) -> &ModifiersShortcutQueue {
        self.user_data().get::<ModifiersShortcutQueue>().unwrap()
    }

    fn last_modifier_change(&self) -> Option<Serial> {
        *self
            .user_data()
            .get::<LastModifierChange>()
            .unwrap()
            .0
            .lock()
            .unwrap()
    }

    fn double_click_tracker(&self) -> &DoubleClickTracker {
        self.user_data().get::<DoubleClickTracker>().unwrap()
    }

    fn cursor_geometry(
        &self,
        loc: impl Into<Point<f64, Buffer>>,
        time: Time<Monotonic>,
    ) -> Option<(Rectangle<i32, Buffer>, Point<i32, Buffer>)> {
        let location = loc.into().to_i32_round();

        match self.cursor_image_status() {
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<Mutex<CursorImageAttributes>>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });
                let geo = bbox_from_surface_tree(&surface, (location.x, location.y));
                let buffer_geo = Rectangle::new(
                    (geo.loc.x, geo.loc.y).into(),
                    geo.size.to_buffer(1, Transform::Normal),
                );
                Some((buffer_geo, (hotspot.x, hotspot.y).into()))
            }
            CursorImageStatus::Named(cursor_icon) => {
                let seat_userdata = self.user_data();
                seat_userdata.insert_if_missing_threadsafe(CursorState::default);
                let state = seat_userdata.get::<CursorState>().unwrap();
                let frame = state
                    .lock()
                    .unwrap()
                    .get_named_cursor(cursor_icon)
                    .get_image(1, time.as_millis());

                Some((
                    Rectangle::new(location, (frame.width as i32, frame.height as i32).into()),
                    (frame.xhot as i32, frame.yhot as i32).into(),
                ))
            }
            CursorImageStatus::Hidden => None,
        }
    }

    fn cursor_image_status(&self) -> CursorImageStatus {
        let lock = self.user_data().get::<Mutex<CursorImageStatus>>().unwrap();
        // Reset the cursor if the surface is no longer alive
        let mut cursor_status = lock.lock().unwrap();
        if let CursorImageStatus::Surface(ref surface) = *cursor_status
            && !surface.alive()
        {
            *cursor_status = CursorImageStatus::default_named();
        }
        cursor_status.clone()
    }

    fn set_cursor_image_status(&self, status: CursorImageStatus) {
        let cursor_status = self.user_data().get::<Mutex<CursorImageStatus>>().unwrap();
        *cursor_status.lock().unwrap() = status;
    }
}
