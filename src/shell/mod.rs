use calloop::LoopHandle;
use focus::target::WindowGroup;
use grabs::{MenuAlignment, SeatMoveGrabState};
use indexmap::IndexMap;
use layout::TilingExceptions;
use std::{
    collections::HashMap,
    sync::{Mutex, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};
use wayland_backend::server::ClientId;

use crate::{
    shell::{focus::FocusTarget, grabs::fullscreen_items, layout::tiling::PlaceholderType},
    wayland::{
        handlers::data_device::{self, get_dnd_icon},
        protocols::workspace::{State as WState, WorkspaceCapabilities},
    },
};
use cosmic_comp_config::{
    AppearanceConfig, TileBehavior, ZoomConfig, ZoomMovement,
    workspace::{PinnedWorkspace, WorkspaceLayout, WorkspaceMode},
};
use cosmic_config::ConfigSet;
use cosmic_protocols::workspace::v2::server::zcosmic_workspace_handle_v2::TilingState;
use cosmic_settings_config::shortcuts::action::{Direction, FocusDirection, ResizeDirection};
use cosmic_settings_config::{shortcuts, window_rules::ApplicationException};
use keyframe::{ease, functions::EaseInOutCubic};
use smithay::{
    backend::{input::TouchSlot, renderer::element::RenderElementStates},
    desktop::{
        LayerSurface, PopupKind, WindowSurface, WindowSurfaceType, layer_map_for_output,
        space::SpaceElement,
        utils::{
            OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, take_presentation_feedback_surface_tree,
        },
    },
    input::{
        Seat,
        pointer::{
            CursorImageStatus, CursorImageSurfaceData, Focus, GrabStartData as PointerGrabStartData,
        },
    },
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{Client, DisplayHandle, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Point, Rectangle, Serial, Size},
    wayland::{
        compositor::{SurfaceAttributes, with_states},
        seat::WaylandFocus,
        session_lock::LockSurface,
        shell::wlr_layer::{KeyboardInteractivity, Layer, LayerSurfaceCachedState},
        xdg_activation::XdgActivationState,
        xwayland_keyboard_grab::XWaylandKeyboardGrab,
    },
    xwayland::X11Surface,
};
use tracing::error;

use crate::{
    backend::render::animations::spring::{Spring, SpringParams},
    config::Config,
    utils::{prelude::*, quirks::WORKSPACE_OVERVIEW_NAMESPACE},
    wayland::{
        handlers::{
            toplevel_management::minimize_rectangle, xdg_activation::ActivationContext,
            xdg_shell::popup::get_popup_toplevel,
        },
        protocols::{
            shell_overlay::{ContextMenuItem, ShellOverlayState},
            toplevel_info::{
                ToplevelInfoState, toplevel_enter_output, toplevel_enter_workspace,
                toplevel_leave_output, toplevel_leave_workspace,
            },
            workspace::{
                WorkspaceGroupHandle, WorkspaceHandle, WorkspaceState, WorkspaceUpdateGuard,
            },
        },
    },
};

pub mod element;
pub mod focus;
pub mod fullscreen_reveal;
pub mod grabs;
pub mod layout;
mod seats;
mod workspace;
pub mod zoom;
pub use self::element::{CosmicMapped, CosmicMappedRenderElement, CosmicSurface};
pub use self::workspace::LayoutMode;
pub use self::seats::*;
pub use self::workspace::*;
use self::zoom::{OutputZoomState, ZoomState};

/// Protocol events queued by Shell, drained by Common::refresh().
#[derive(Debug)]
pub(crate) enum ZoomProtocolEvent {
    Show { level: f64, increment: u32, movement: u32 },
    Update { level: f64 },
    Hide,
}

use self::{
    element::{
        CosmicWindow, MaximizedState,
        resize_indicator::{ResizeIndicator, resize_indicator},
        swap_indicator::{SwapIndicator, swap_indicator},
    },
    focus::target::{KeyboardFocusTarget, PointerFocusTarget},
    grabs::{
        GrabStartData, Item, MenuGrab, MoveGrab, ReleaseMode, ResizeEdge, ResizeGrab, tab_items,
        window_items,
    },
    layout::{
        floating::{FloatingLayout, ResizeState},
        tiling::{NodeDesc, ResizeForkGrab, TilingLayout},
    },
};

const ANIMATION_DURATION: Duration = Duration::from_millis(200);
const GESTURE_MAX_LENGTH: f64 = 150.0;
const GESTURE_POSITION_THRESHOLD: f64 = 0.5;
const GESTURE_VELOCITY_THRESHOLD: f64 = 0.02;
const MOVE_GRAB_Y_OFFSET: f64 = 16.;
const ACTIVATION_TOKEN_EXPIRE_TIME: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum Trigger {
    KeyboardSwap(shortcuts::Binding, NodeDesc),
    KeyboardMove(shortcuts::Modifiers),
    Pointer(u32),
    Touch(TouchSlot),
}

#[derive(Debug, Clone)]
pub enum OverviewMode {
    None,
    Started(Trigger, Instant),
    Active(Trigger),
    Ended(Option<Trigger>, Instant),
}

impl OverviewMode {
    pub fn alpha(&self) -> Option<f32> {
        match self {
            OverviewMode::Started(_, start) => {
                let percentage = Instant::now().duration_since(*start).as_millis() as f32
                    / ANIMATION_DURATION.as_millis() as f32;
                Some(ease(EaseInOutCubic, 0.0, 1.0, percentage))
            }
            OverviewMode::Active(_) => Some(1.0),
            OverviewMode::Ended(_, end) => {
                let percentage = Instant::now().duration_since(*end).as_millis() as f32
                    / ANIMATION_DURATION.as_millis() as f32;
                if percentage < 1.0 {
                    Some(ease(EaseInOutCubic, 1.0, 0.0, percentage))
                } else {
                    None
                }
            }
            OverviewMode::None => None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, OverviewMode::Started(_, _) | OverviewMode::Active(_))
    }

    pub fn active_trigger(&self) -> Option<&Trigger> {
        if let OverviewMode::Started(trigger, _) | OverviewMode::Active(trigger) = self {
            Some(trigger)
        } else {
            None
        }
    }

    pub fn trigger(&self) -> Option<&Trigger> {
        self.active_trigger().or({
            if let OverviewMode::Ended(trigger, _) = self {
                trigger.as_ref()
            } else {
                None
            }
        })
    }
}

#[derive(Debug, Clone)]
pub enum ResizeMode {
    None,
    Started(shortcuts::Binding, Instant, ResizeDirection),
    Active(shortcuts::Binding, ResizeDirection),
    Ended(Instant, ResizeDirection),
}

impl ResizeMode {
    pub fn alpha(&self) -> Option<f32> {
        match self {
            ResizeMode::Started(_, start, _) => {
                let percentage = Instant::now().duration_since(*start).as_millis() as f32
                    / ANIMATION_DURATION.as_millis() as f32;
                Some(ease(EaseInOutCubic, 0.0, 1.0, percentage))
            }
            ResizeMode::Active(_, _) => Some(1.0),
            ResizeMode::Ended(end, _) => {
                let percentage = Instant::now().duration_since(*end).as_millis() as f32
                    / ANIMATION_DURATION.as_millis() as f32;
                if percentage < 1.0 {
                    Some(ease(EaseInOutCubic, 1.0, 0.0, percentage))
                } else {
                    None
                }
            }
            ResizeMode::None => None,
        }
    }

    pub fn active_binding(&self) -> Option<&shortcuts::Binding> {
        if let ResizeMode::Started(binding, _, _) | ResizeMode::Active(binding, _) = self {
            Some(binding)
        } else {
            None
        }
    }

    pub fn active_direction(&self) -> Option<ResizeDirection> {
        if let ResizeMode::Started(_, _, direction) | ResizeMode::Active(_, direction) = self {
            Some(*direction)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActivationKey {
    Wayland(WlSurface),
    X11(u32),
}

impl From<&CosmicSurface> for ActivationKey {
    fn from(value: &CosmicSurface) -> Self {
        match value.0.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                ActivationKey::Wayland(toplevel.wl_surface().clone())
            }
            WindowSurface::X11(s) => ActivationKey::X11(s.window_id()),
        }
    }
}

#[derive(Debug)]
pub struct PendingWindow {
    pub surface: CosmicSurface,
    pub seat: Seat<State>,
    pub fullscreen: Option<Output>,
    pub maximized: bool,
}

/// Metadata captured at minimize-time, returned from
/// `Shell::minimize_request` so callers can forward it to the event
/// bus without having to re-resolve the window's identity after it's
/// been moved into the minimized-windows list.
#[derive(Debug, Clone)]
pub struct MinimizedInfo {
    pub window_id: String,
    pub app_id: String,
    pub title: String,
    pub workspace_id: String,
}

/// Counterpart to `MinimizedInfo`, returned from `unminimize_request`.
/// Only carries the identifiers needed for the `window.unminimized`
/// event — title/workspace are not re-emitted on restore.
#[derive(Debug, Clone)]
pub struct UnminimizedInfo {
    pub window_id: String,
    pub app_id: String,
}

#[derive(Debug)]
pub struct PendingLayer {
    pub surface: LayerSurface,
    pub seat: Seat<State>,
    pub output: Output,
}

/// State for the scratchpad: hidden floating windows that can be
/// toggled on/off instantly with a keybinding.
#[derive(Debug, Default)]
pub struct ScratchpadState {
    /// Windows stored in the scratchpad (may be hidden or visible).
    pub windows: Vec<CosmicMapped>,
    /// Index of the currently visible scratchpad window, or None if all hidden.
    pub visible: Option<usize>,
}

#[derive(Debug)]
pub struct Shell {
    pub workspaces: Workspaces,

    // Can't make this into a HashSet. See https://github.com/pop-os/cosmic-comp/pull/1902
    pub pending_windows: Vec<PendingWindow>,
    pub pending_layers: Vec<PendingLayer>,
    pub pending_activations: HashMap<ActivationKey, ActivationContext>,
    pub override_redirect_windows: Vec<X11Surface>,
    pub session_lock: Option<SessionLock>,
    pub seats: Seats,
    pub previous_workspace_idx: Option<(Serial, WeakOutput, usize)>,
    pub xwayland_keyboard_grab: Option<XWaylandKeyboardGrab<State>>,

    pub active_hint: bool,
    /// Runtime mirror of `compositor.toml [layout] tiled_headers`.
    /// Read on `should_render_window_header` and `has_ssd` to
    /// decide whether single tiled SSD windows render the
    /// compositor-rendered Lunaris header. Updated when the
    /// compositor.toml watcher fires (see `toml_config_changed`)
    /// and we then `set_tiled_headers_enabled` which schedules a
    /// reconfigure of every tiled mapped window. Default `false`
    /// — tiling-WM convention.
    pub tiled_headers_enabled: bool,
    pub lunaris_theme: lunaris_theme::LunarisTheme,
    overview_mode: OverviewMode,
    swap_indicator: Option<SwapIndicator>,
    resize_mode: ResizeMode,
    resize_state: Option<(
        KeyboardFocusTarget,
        ResizeDirection,
        ResizeEdge,
        i32,
        usize,
        Output,
    )>,
    resize_indicator: Option<ResizeIndicator>,
    /// Indicator kinds to hide, drained by `Common::refresh()`.
    pub(crate) pending_indicator_hides: Vec<u32>,
    pub(crate) pending_zoom_event: Option<ZoomProtocolEvent>,
    zoom_state: Option<ZoomState>,
    appearance_conf: AppearanceConfig,
    tiling_exceptions: TilingExceptions,
    /// TOML-based window rules (float/tile).
    pub(crate) window_rules: Vec<crate::config::WindowRule>,
    /// Scratchpad: hidden windows toggled on/off with a keybinding.
    pub scratchpad: ScratchpadState,

    #[cfg(feature = "debug")]
    pub debug_active: bool,
}

#[derive(Debug)]
pub struct SessionLock {
    pub ext_session_lock: ExtSessionLockV1,
    pub surfaces: HashMap<Output, LockSurface>,
}

#[derive(Debug, Clone, Copy)]
pub enum WorkspaceDelta {
    Shortcut(Instant),
    Gesture {
        percentage: f64,
        forward: bool,
    },
    GestureEnd {
        start: Instant,
        spring: Spring,
        forward: bool,
    },
    // InvalidGesture(f64), TODO
    // InvalidGestureEnd(Instant, Spring), TODO
}

impl WorkspaceDelta {
    pub fn new_gesture(forward: bool) -> Self {
        WorkspaceDelta::Gesture {
            percentage: 0.0,
            forward,
        }
    }

    pub fn new_gesture_end(delta: f64, velocity: f64, forward: bool) -> Self {
        let params: SpringParams = SpringParams::new(1.0, 1000.0, 0.0001);
        WorkspaceDelta::GestureEnd {
            start: Instant::now(),
            forward,
            spring: Spring {
                from: delta,
                to: 1.0,
                initial_velocity: velocity,
                params,
            },
        }
    }

    pub fn new_shortcut() -> Self {
        WorkspaceDelta::Shortcut(Instant::now())
    }

    pub fn is_animating(&self) -> bool {
        matches!(
            self,
            WorkspaceDelta::Shortcut(_) | WorkspaceDelta::GestureEnd { .. }
        )
    }
}

#[derive(Debug)]
pub struct WorkspaceSet {
    previously_active: Option<(usize, WorkspaceDelta)>,
    pub active: usize,
    pub group: WorkspaceGroupHandle,
    tiling_enabled: bool,
    output: Output,
    appearance: AppearanceConfig,
    pub sticky_layer: FloatingLayout,
    pub minimized_windows: Vec<MinimizedWindow>,
    pub workspaces: Vec<Workspace>,
}

fn create_workspace(
    state: &mut WorkspaceUpdateGuard<'_, State>,
    output: &Output,
    group_handle: &WorkspaceGroupHandle,
    active: bool,
    tiling: bool,
    appearance: AppearanceConfig,
) -> Workspace {
    let workspace_handle = state
        .create_workspace(
            group_handle,
            if tiling {
                TilingState::TilingEnabled
            } else {
                TilingState::FloatingOnly
            },
            // TODO Set id for persistent workspaces
            None,
        )
        .unwrap();
    if active {
        state.add_workspace_state(&workspace_handle, WState::Active);
    }
    state.set_workspace_capabilities(
        &workspace_handle,
        WorkspaceCapabilities::Activate
            | WorkspaceCapabilities::SetTilingState
            | WorkspaceCapabilities::Pin
            | WorkspaceCapabilities::Move,
    );
    Workspace::new(
        workspace_handle,
        output.clone(),
        tiling,
        appearance,
    )
}

fn create_workspace_from_pinned(
    pinned: &PinnedWorkspace,
    state: &mut WorkspaceUpdateGuard<'_, State>,
    output: &Output,
    group_handle: &WorkspaceGroupHandle,
    active: bool,
    appearance: AppearanceConfig,
) -> Workspace {
    let workspace_handle = state
        .create_workspace(
            group_handle,
            if pinned.tiling_enabled {
                TilingState::TilingEnabled
            } else {
                TilingState::FloatingOnly
            },
            pinned.id.clone(),
        )
        .unwrap();
    state.add_workspace_state(&workspace_handle, WState::Pinned);
    if active {
        state.add_workspace_state(&workspace_handle, WState::Active);
    }
    state.set_workspace_capabilities(
        &workspace_handle,
        WorkspaceCapabilities::Activate
            | WorkspaceCapabilities::SetTilingState
            | WorkspaceCapabilities::Pin
            | WorkspaceCapabilities::Move,
    );
    Workspace::from_pinned(
        pinned,
        workspace_handle,
        output.clone(),
        appearance,
    )
}

/* We will probably need this again at some point
fn merge_workspaces(
    mut workspace: Workspace,
    into: &mut Workspace,
    workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    toplevel_info_state: &mut ToplevelInfoState<State, CosmicSurface>,
) {
    if into.fullscreen.is_some() {
        // Don't handle the returned original workspace, for this nieche case.
        let _ = workspace.remove_fullscreen();
    }

    for element in workspace.mapped() {
        // fixup toplevel state
        for (toplevel, _) in element.windows() {
            toplevel_info_state.toplevel_leave_workspace(&toplevel, &workspace.handle);
            toplevel_info_state.toplevel_enter_workspace(&toplevel, &into.handle);
        }
    }
    // TODO: merge minimized windows
    into.tiling_layer.merge(workspace.tiling_layer);
    into.floating_layer.merge(workspace.floating_layer);
    workspace_state.remove_workspace(workspace.handle);
}
*/

impl WorkspaceSet {
    fn new(
        state: &mut WorkspaceUpdateGuard<'_, State>,
        output: &Output,
        tiling_enabled: bool,
        appearance: AppearanceConfig,
    ) -> WorkspaceSet {
        let group_handle = state.create_workspace_group();
        let sticky_layer = FloatingLayout::new(appearance, output);

        WorkspaceSet {
            previously_active: None,
            active: 0,
            group: group_handle,
            tiling_enabled,
            sticky_layer,
            minimized_windows: Vec::new(),
            workspaces: Vec::new(),
            output: output.clone(),
            appearance,
        }
    }

    fn activate(
        &mut self,
        idx: usize,
        workspace_delta: WorkspaceDelta,
        state: &mut WorkspaceUpdateGuard<'_, State>,
    ) -> Result<bool, InvalidWorkspaceIndex> {
        if idx >= self.workspaces.len() {
            return Err(InvalidWorkspaceIndex);
        }

        // Animate if workspaces overview isn't open
        let layer_map = layer_map_for_output(&self.output);
        let animate = !layer_map
            .layers()
            .any(|l| l.namespace() == WORKSPACE_OVERVIEW_NAMESPACE);

        if self.active != idx {
            let old_active = self.active;
            state.remove_workspace_state(&self.workspaces[old_active].handle, WState::Active);
            state.remove_workspace_state(&self.workspaces[old_active].handle, WState::Urgent);
            state.remove_workspace_state(&self.workspaces[idx].handle, WState::Urgent);
            state.add_workspace_state(&self.workspaces[idx].handle, WState::Active);
            self.previously_active = if animate {
                Some((old_active, workspace_delta))
            } else {
                None
            };
            self.active = idx;
            Ok(true)
        } else {
            // snap to workspace, when in between workspaces due to swipe gesture
            if let Some((p_idx, p_delta)) = self.previously_active {
                if matches!(p_delta, WorkspaceDelta::Gesture { .. })
                    && matches!(workspace_delta, WorkspaceDelta::GestureEnd { .. })
                {
                    self.previously_active = Some((p_idx, workspace_delta));
                } else {
                    self.previously_active = None;
                }
                return Ok(true);
            }
            Ok(false)
        }
    }

    fn activate_previous(
        &mut self,
        workspace_delta: WorkspaceDelta,
        state: &mut WorkspaceUpdateGuard<'_, State>,
    ) -> Result<bool, InvalidWorkspaceIndex> {
        if let Some((idx, _)) = self.previously_active {
            return self.activate(idx, workspace_delta, state);
        }
        Err(InvalidWorkspaceIndex)
    }

    fn update_workspace_delta(&mut self, delta: f64, forward: bool) {
        let easing = delta.clamp(0.0, GESTURE_MAX_LENGTH).abs() / GESTURE_MAX_LENGTH;
        if let Some((idx, _)) = self.previously_active {
            self.previously_active = Some((
                idx,
                WorkspaceDelta::Gesture {
                    percentage: easing,
                    forward,
                },
            ));
        }
    }

    fn set_output(&mut self, new_output: &Output, explicit: bool) {
        self.sticky_layer.set_output(new_output);
        for window in self.sticky_layer.windows() {
            toplevel_leave_output(&window, &self.output);
            toplevel_enter_output(&window, new_output);
        }
        for workspace in &mut self.workspaces {
            workspace.set_output(new_output, explicit);
        }
        self.output = new_output.clone();
    }

    fn refresh(&mut self) {
        if let Some((_, start)) = self.previously_active {
            match start {
                WorkspaceDelta::Shortcut(st) => {
                    if Instant::now().duration_since(st).as_millis() as f32
                        >= ANIMATION_DURATION.as_millis() as f32
                    {
                        self.previously_active = None;
                    }
                }
                WorkspaceDelta::GestureEnd { start, spring, .. } => {
                    if Instant::now().duration_since(start).as_millis()
                        > spring.duration().as_millis()
                    {
                        self.previously_active = None;
                    }
                }
                _ => {}
            }
        } else {
            self.workspaces[self.active].refresh();
        }
        self.sticky_layer.refresh();
    }

    fn add_empty_workspace(&mut self, state: &mut WorkspaceUpdateGuard<State>) {
        let workspace = create_workspace(
            state,
            &self.output,
            &self.group,
            false,
            self.tiling_enabled,
            self.appearance,
        );
        workspace_set_idx(
            state,
            self.workspaces.len() as u8 + 1,
            &workspace.handle,
            // this method is only used by code paths related to dynamic workspaces, so this should be fine
        );
        self.workspaces.push(workspace);
    }

    fn ensure_last_empty(
        &mut self,
        state: &mut WorkspaceUpdateGuard<State>,
        xdg_activation_state: &XdgActivationState,
    ) {
        // add empty at the end, if necessary
        if self
            .workspaces
            .last()
            .is_none_or(|last| !last.is_empty() || last.pinned)
        {
            self.add_empty_workspace(state);
        }

        // Remove empty workspaces only when safe for stable position labels.
        //
        // Lunaris deviates from upstream cosmic here: the top-bar indicator
        // labels workspaces by their position in the list (1, 2, 3, ...), so
        // trimming an empty workspace BEFORE the active one silently shifts
        // every subsequent label — the user clicks "Move to Workspace 2",
        // sees the "2" pill light up for an instant, then watches the pill
        // collapse back to "1" once the vacated source workspace is reaped.
        //
        // Rule: keep every empty workspace at or before `active`, plus the
        // trailing empty spare. Also keep the workspace that a running
        // activate-animation references (`previously_active`), so the slide
        // has something to interpolate from until the animation settles.
        // Empty workspaces in the "gap" between the active one and the
        // trailing spare are reaped as in upstream so users don't accumulate
        // dead slots by moving windows around.
        let len = self.workspaces.len();
        let kept: Vec<bool> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, workspace)| {
                let keep = if workspace.can_auto_remove(xdg_activation_state) {
                    i <= self.active
                        || i == len - 1
                        || self
                            .previously_active
                            .is_some_and(|(p_idx, _)| p_idx == i)
                } else {
                    true
                };
                if !keep {
                    state.remove_workspace(workspace.handle);
                }
                keep
            })
            .collect();

        let mut iter = kept.iter();
        self.workspaces.retain(|_| *iter.next().unwrap());
        self.active -= kept
            .iter()
            .take(self.active + 1)
            .filter(|kept| !**kept)
            .count();

        // `previously_active` holds the index of the workspace the user is
        // animating AWAY FROM. If we just removed that workspace, the
        // animation source no longer exists — clear it so the renderer
        // snaps straight to `active` instead of interpolating against a
        // stale index that now points at a different workspace. If the
        // reference workspace survived but earlier workspaces were
        // removed, shift the index down to keep pointing at the same
        // workspace. Without this correction, `shell.move_element` +
        // `follow=true` + auto-cleanup of the now-empty source workspace
        // would leave `previously_active.idx` aliasing the target
        // workspace, and the slide animation would visually start and
        // end on the same position (user sees: window moves forward,
        // then "springs back").
        if let Some((p_idx, p_delta)) = self.previously_active {
            if !kept.get(p_idx).copied().unwrap_or(false) {
                self.previously_active = None;
            } else {
                let shift = kept.iter().take(p_idx).filter(|k| !**k).count();
                self.previously_active = Some((p_idx - shift, p_delta));
            }
        }

        if kept.iter().any(|val| !(*val)) {
            self.update_workspace_idxs(state);
        }
    }

    fn update_workspace_idxs(&self, state: &mut WorkspaceUpdateGuard<'_, State>) {
        for (i, workspace) in self.workspaces.iter().enumerate() {
            workspace_set_idx(state, i as u8 + 1, &workspace.handle);
        }
    }

    fn post_remove_workspace(
        &mut self,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        previous_active_handle: &WorkspaceHandle,
    ) {
        if self.workspaces.is_empty() {
            self.add_empty_workspace(workspace_state);
        }
        self.update_workspace_idxs(workspace_state);
        self.active = self
            .workspaces
            .iter()
            .position(|w| w.handle == *previous_active_handle)
            .unwrap_or_else(|| {
                let idx = self.workspaces.len() - 1;
                let workspace = &self.workspaces[idx];
                workspace_state.add_workspace_state(&workspace.handle, WState::Active);
                idx
            });
    }

    // Remove a workspace from the set, and return it, for adding to a different
    // workspace set
    fn remove_workspace(
        &mut self,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        handle: &WorkspaceHandle,
    ) -> Option<Workspace> {
        let previous_active_handle = self.workspaces[self.active].handle;
        let idx = self.workspaces.iter().position(|w| w.handle == *handle)?;
        let workspace = self.workspaces.remove(idx);
        self.post_remove_workspace(workspace_state, &previous_active_handle);
        Some(workspace)
    }

    // Remove all workspaces matched by the callback from the set
    fn remove_workspaces(
        &mut self,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        cb: impl Fn(&Workspace) -> bool,
    ) -> Vec<Workspace> {
        let previous_active_handle = self.workspaces[self.active].handle;
        let (prefers, doesnt) = self.workspaces.drain(..).partition(cb);
        self.workspaces = doesnt;
        self.post_remove_workspace(workspace_state, &previous_active_handle);
        prefers
    }
}

#[derive(Debug)]
pub struct Workspaces {
    pub sets: IndexMap<Output, WorkspaceSet>,
    backup_set: Option<WorkspaceSet>,
    pub layout: WorkspaceLayout,
    mode: WorkspaceMode,
    autotile: bool,
    autotile_behavior: TileBehavior,
    appearance: AppearanceConfig,
    // Persisted workspace to add on first `output_add`
    persisted_workspaces: Vec<PinnedWorkspace>,
}

impl Workspaces {
    pub fn new(config: &Config) -> Workspaces {
        Workspaces {
            sets: IndexMap::new(),
            backup_set: None,
            layout: config.cosmic_conf.workspaces.workspace_layout,
            mode: config.cosmic_conf.workspaces.workspace_mode,
            autotile: config.cosmic_conf.autotile,
            autotile_behavior: config.cosmic_conf.autotile_behavior,
            appearance: config.cosmic_conf.appearance_settings,
            persisted_workspaces: config.cosmic_conf.pinned_workspaces.clone(),
        }
    }

    pub fn add_output(
        &mut self,
        output: &Output,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        if self.sets.contains_key(output) {
            return;
        }

        let mut set = self
            .backup_set
            .take()
            .map(|mut set| {
                set.set_output(output, false);
                set
            })
            .unwrap_or_else(|| {
                WorkspaceSet::new(
                    workspace_state,
                    output,
                    self.autotile,
                    self.appearance,
                )
            });
        workspace_state.add_group_output(&set.group, output);

        // If this is the first output added, create workspaces for pinned workspaces from config
        for pinned in std::mem::take(&mut self.persisted_workspaces) {
            let workspace = create_workspace_from_pinned(
                &pinned,
                workspace_state,
                output,
                &set.group,
                false,
                self.appearance,
            );
            set.workspaces.push(workspace);
        }

        // Remove workspaces that prefer this output from other sets
        let mut moved_workspaces = self
            .sets
            .values_mut()
            .flat_map(|other_set| {
                other_set.remove_workspaces(workspace_state, |w| w.prefers_output(output))
            })
            .collect::<Vec<_>>();

        // Add `moved_workspaces` to set, and update output and index of workspaces
        for workspace in &mut moved_workspaces {
            workspace_state.remove_workspace_state(&workspace.handle, WState::Active);
            workspace_state.move_workspace_to_group(set.group, workspace.handle);
        }
        set.workspaces.extend(moved_workspaces);
        if set.workspaces.is_empty() {
            set.add_empty_workspace(workspace_state);
        }
        set.update_workspace_idxs(workspace_state);
        for (i, workspace) in set.workspaces.iter_mut().enumerate() {
            workspace.set_output(output, false);
            workspace.refresh();
            if i == set.active {
                workspace_state.add_workspace_state(&workspace.handle, WState::Active);
            }
        }
        self.sets.insert(output.clone(), set);
    }

    pub fn remove_output<'a>(
        &mut self,
        output: &Output,
        seats: impl Iterator<Item = &'a Seat<State>>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        xdg_activation_state: &XdgActivationState,
    ) {
        if !self.sets.contains_key(output) {
            return;
        }

        if let Some(set) = self.sets.shift_remove(output) {
            {
                let map = layer_map_for_output(output);
                for surface in map.layers() {
                    surface.layer_surface().send_close();
                }
            }

            // TODO: Heuristic which output to move to.
            // It is supposed to be the *most* internal, we just pick the first one for now
            // and hope enumeration order works in our favor.
            let new_output = self.sets.get_index(0).map(|(o, _)| o.clone());
            if let Some(new_output) = new_output {
                for seat in seats {
                    if &seat.active_output() == output {
                        seat.set_active_output(&new_output);
                    }
                    if seat.focused_output().as_ref() == Some(output) {
                        seat.set_focused_output(None);
                    }
                }

                let new_set = self.sets.get_mut(&new_output).unwrap();
                let workspace_group = new_set.group;
                for (i, mut workspace) in set.workspaces.into_iter().enumerate() {
                    if workspace.can_auto_remove(xdg_activation_state) {
                        workspace_state.remove_workspace(workspace.handle);
                    } else {
                        // update workspace protocol state
                        workspace_state.remove_workspace_state(&workspace.handle, WState::Active);
                        workspace_state.move_workspace_to_group(workspace_group, workspace.handle);

                        // update mapping
                        workspace.set_output(&new_output, false);
                        workspace.refresh();
                        new_set.workspaces.push(workspace);

                        // If workspace was active, and the new set's active workspace is empty, make this workspace
                        // active on the new set. Instead of leaving an empty workspace active, and a previously active
                        // workspace hidden.
                        if i == set.active && new_set.workspaces[new_set.active].is_empty() {
                            workspace_state.remove_workspace_state(
                                &new_set.workspaces[new_set.active].handle,
                                WState::Active,
                            );
                            new_set.active = new_set.workspaces.len() - 1;
                            workspace_state.add_workspace_state(
                                &new_set.workspaces[new_set.active].handle,
                                WState::Active,
                            );
                        }
                    }
                }

                for window in set.sticky_layer.mapped() {
                    for (surface, _) in window.windows() {
                        toplevel_leave_output(&surface, output);
                        toplevel_enter_output(&surface, &new_output);
                    }
                }
                new_set.sticky_layer.merge(set.sticky_layer);
                for window in set.minimized_windows.iter() {
                    for surface in window.windows() {
                        toplevel_leave_output(&surface, output);
                        toplevel_enter_output(&surface, &new_output);
                    }
                }
                new_set.minimized_windows.extend(set.minimized_windows);

                if self.mode == WorkspaceMode::OutputBound {
                    workspace_state.remove_workspace_group(set.group);
                } else {
                    workspace_state.remove_group_output(&workspace_group, output);
                }
            } else {
                workspace_state.remove_group_output(&set.group, output);
                self.backup_set = Some(set);
            }

            self.refresh(workspace_state, xdg_activation_state)
        }
    }

    // Move workspace from one output to another, explicitly by the user
    pub fn migrate_workspace(
        &mut self,
        from: &Output,
        to: &Output,
        handle: &WorkspaceHandle,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        // If workspaces span across outputs, they can't be moved to a different output
        if self.mode == WorkspaceMode::Global {
            return;
        }

        if !self.sets.contains_key(to) || from == to {
            return;
        }

        if let Some(mut workspace) = self
            .sets
            .get_mut(from)
            .and_then(|set| set.remove_workspace(workspace_state, handle))
        {
            let new_set = self.sets.get_mut(to).unwrap();
            workspace_state.remove_workspace_state(&workspace.handle, WState::Active);
            workspace_state.move_workspace_to_group(new_set.group, workspace.handle);
            workspace.set_output(to, true);
            workspace.refresh();
            new_set.workspaces.insert(new_set.active + 1, workspace);
            new_set.update_workspace_idxs(workspace_state);
        }
    }

    // Move a workspace before/after a different workspace
    pub fn move_workspace(
        &mut self,
        handle: &WorkspaceHandle,
        other_handle: &WorkspaceHandle,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        after: bool,
    ) {
        if handle == other_handle {
            return;
        }

        let (Some(old_output), Some(new_output)) = (
            self.space_for_handle(handle).map(|w| w.output.clone()),
            self.space_for_handle(other_handle)
                .map(|w| w.output.clone()),
        ) else {
            return;
        };

        match self.mode {
            WorkspaceMode::OutputBound => {
                // Check which workspace is active on the new set; before removing from the
                // old set in cause we're moving an active workspace within the same set.
                let new_set = &mut self.sets[&new_output];
                let previous_active_handle = new_set.workspaces[new_set.active].handle;

                // Remove workspace from old set
                let old_set = &mut self.sets[&old_output];
                let mut workspace = if new_output != old_output {
                    old_set.remove_workspace(workspace_state, handle).unwrap()
                } else {
                    // If set is the same, just remove it here without adding empty workspace,
                    // updating `active`, etc.
                    let idx = old_set
                        .workspaces
                        .iter()
                        .position(|w| w.handle == *handle)
                        .unwrap();
                    old_set.workspaces.remove(idx)
                };

                let new_set = &mut self.sets[&new_output];

                if new_output != old_output {
                    workspace_state.remove_workspace_state(&workspace.handle, WState::Active);
                    workspace_state.move_workspace_to_group(new_set.group, workspace.handle);
                    workspace.set_output(&new_output, true);
                    workspace.refresh();
                }

                // Insert workspace into new set, relative to `other_handle`
                let idx = new_set
                    .workspaces
                    .iter()
                    .position(|w| w.handle == *other_handle)
                    .unwrap();
                let insert_idx = if after { idx + 1 } else { idx };
                new_set.workspaces.insert(insert_idx, workspace);

                new_set.active = new_set
                    .workspaces
                    .iter()
                    .position(|w| w.handle == previous_active_handle)
                    .unwrap();

                new_set.update_workspace_idxs(workspace_state);
            }
            WorkspaceMode::Global => {
                let old_set = &mut self.sets[&old_output];
                let old_idx = old_set
                    .workspaces
                    .iter()
                    .position(|w| w.handle == *handle)
                    .unwrap();

                let new_set = &mut self.sets[&new_output];
                let other_idx = new_set
                    .workspaces
                    .iter()
                    .position(|w| w.handle == *other_handle)
                    .unwrap();

                // Move workspace at given index on every output
                for set in self.sets.values_mut() {
                    if old_idx < set.workspaces.len() && other_idx < set.workspaces.len() {
                        let previous_active_handle = set.workspaces[set.active].handle;

                        if other_idx > old_idx {
                            let insert_idx = if after { other_idx } else { other_idx - 1 };
                            set.workspaces[old_idx..=insert_idx].rotate_left(1);
                        } else {
                            let insert_idx = if after { other_idx + 1 } else { other_idx };
                            set.workspaces[insert_idx..=old_idx].rotate_right(1);
                        }

                        set.active = set
                            .workspaces
                            .iter()
                            .position(|w| w.handle == previous_active_handle)
                            .unwrap();

                        set.update_workspace_idxs(workspace_state);
                    }
                }
            }
        }
    }

    pub fn update_config(
        &mut self,
        config: &Config,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        xdg_activation_state: &XdgActivationState,
    ) {
        let old_mode = self.mode;
        self.mode = config.cosmic_conf.workspaces.workspace_mode;
        self.layout = config.cosmic_conf.workspaces.workspace_layout;
        self.appearance = config.cosmic_conf.appearance_settings;

        for set in self.sets.values_mut() {
            set.appearance = self.appearance;
            set.sticky_layer.appearance = self.appearance;
            for workspace in set.workspaces.iter_mut() {
                workspace.floating_layer.appearance = self.appearance;
                workspace.tiling_layer.appearance = self.appearance;
            }
        }

        if self.sets.len() <= 1 {
            return;
        }

        match (old_mode, self.mode) {
            (WorkspaceMode::Global, WorkspaceMode::OutputBound) => {
                // We basically just unlink the existing spaces, so nothing needs to be updated
            }
            (WorkspaceMode::OutputBound, WorkspaceMode::Global) => {
                // lets construct an iterator of all the pairs of workspaces we have to "merge"
                let mut pairs = Vec::new();
                if let Some(max) = self.sets.values().map(|set| set.workspaces.len()).max() {
                    let offset = self.sets.values().map(|set| set.active).max().unwrap();
                    for i in 0..max {
                        pairs.push(
                            self.sets
                                .values()
                                .map(|set| {
                                    let idx = set.active as isize + i as isize - offset as isize;
                                    if idx < 0 || idx >= set.workspaces.len() as isize {
                                        None
                                    } else {
                                        Some(idx)
                                    }
                                })
                                .collect::<Vec<_>>(),
                        );
                    }
                }

                for (j, pair) in pairs.iter().enumerate() {
                    for (i, x) in pair.iter().enumerate() {
                        // Fill up sets, where necessary
                        if x.is_none() {
                            // create missing workspace
                            let (output, set) = self.sets.get_index_mut(i).unwrap();
                            set.workspaces.insert(
                                j,
                                create_workspace(
                                    workspace_state,
                                    output,
                                    &set.group,
                                    false,
                                    config.cosmic_conf.autotile,
                                    self.appearance,
                                ),
                            );
                        }
                        // Otherwise we are fine
                    }
                }
            }
            _ => {}
        };

        self.refresh(workspace_state, xdg_activation_state)
    }

    pub fn recalculate(&mut self) {
        for set in self.sets.values_mut() {
            set.sticky_layer.recalculate();
            set.workspaces.iter_mut().for_each(|w| w.recalculate());
        }
    }

    pub fn refresh(
        &mut self,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        xdg_activation_state: &XdgActivationState,
    ) {
        match self.mode {
            WorkspaceMode::Global => {
                let Some(max) = self.sets.values().map(|set| set.workspaces.len()).max() else {
                    return;
                };

                for set in self
                    .sets
                    .values_mut()
                    .filter(|set| set.workspaces.len() < max)
                {
                    while set.workspaces.len() < max {
                        set.add_empty_workspace(workspace_state)
                    }
                }

                // add empty at the end, if necessary
                if self
                    .sets
                    .values()
                    .flat_map(|set| set.workspaces.last())
                    .any(|w| !w.is_empty() || w.pinned)
                {
                    for set in self.sets.values_mut() {
                        set.add_empty_workspace(workspace_state);
                    }
                }

                // remove empty workspaces in between, if they are not active
                let len = self.sets[0].workspaces.len();
                let mut active = self.sets[0].active;
                let mut keep = vec![true; len];
                for i in 0..len {
                    let has_windows = self
                        .sets
                        .values()
                        .any(|s| !s.workspaces[i].can_auto_remove(xdg_activation_state));

                    if !has_windows && i != active && i != len - 1 {
                        for workspace in self.sets.values().map(|s| &s.workspaces[i]) {
                            workspace_state.remove_workspace(workspace.handle);
                        }
                        keep[i] = false;
                    }
                }

                self.sets.values_mut().for_each(|s| {
                    let mut iter = keep.iter();
                    s.workspaces.retain(|_| *iter.next().unwrap());
                });
                active -= keep.iter().take(active + 1).filter(|keep| !**keep).count();
                self.sets.values_mut().for_each(|s| {
                    s.active = active;
                });

                if keep.iter().any(|val| !(*val)) {
                    for set in self.sets.values_mut() {
                        set.update_workspace_idxs(workspace_state);
                    }
                }
            }
            WorkspaceMode::OutputBound => {
                for set in self.sets.values_mut() {
                    set.ensure_last_empty(workspace_state, xdg_activation_state);
                }
            }
        }

        for set in self.sets.values_mut() {
            set.refresh()
        }
    }

    pub fn get(&self, num: usize, output: &Output) -> Option<&Workspace> {
        self.sets
            .get(output)
            .and_then(|set| set.workspaces.get(num))
    }

    pub fn get_mut(&mut self, num: usize, output: &Output) -> Option<&mut Workspace> {
        self.sets
            .get_mut(output)
            .and_then(|set| set.workspaces.get_mut(num))
    }

    pub fn active(
        &self,
        output: &Output,
    ) -> Option<(Option<(&Workspace, WorkspaceDelta)>, &Workspace)> {
        self.sets
            .get(output)
            .or(self.backup_set.as_ref())
            .map(|set| {
                (
                    set.previously_active.and_then(|(idx, start)| {
                        set.workspaces.get(idx).map(|previous| (previous, start))
                    }),
                    &set.workspaces[set.active],
                )
            })
    }

    pub fn active_mut(&mut self, output: &Output) -> Option<&mut Workspace> {
        self.sets
            .get_mut(output)
            .or(self.backup_set.as_mut())
            .map(|set| &mut set.workspaces[set.active])
    }

    pub fn active_num(&self, output: &Output) -> (Option<usize>, usize) {
        let set = self.sets.get(output).or(self.backup_set.as_ref()).unwrap();
        (set.previously_active.map(|(idx, _)| idx), set.active)
    }

    pub fn idx_for_handle(&self, output: &Output, handle: &WorkspaceHandle) -> Option<usize> {
        let set = self.sets.get(output).unwrap();
        set.workspaces
            .iter()
            .enumerate()
            .find_map(|(i, w)| (&w.handle == handle).then_some(i))
    }

    pub fn len(&self, output: &Output) -> usize {
        let set = self.sets.get(output).unwrap();
        set.workspaces.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Output, &WorkspaceSet)> {
        self.sets.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&Output, &mut WorkspaceSet)> {
        self.sets.iter_mut()
    }

    pub fn spaces(&self) -> impl Iterator<Item = &Workspace> {
        self.sets.values().flat_map(|set| set.workspaces.iter())
    }

    pub fn space_for_handle(&self, handle: &WorkspaceHandle) -> Option<&Workspace> {
        self.spaces().find(|w| &w.handle == handle)
    }

    pub fn space_for_handle_mut(&mut self, handle: &WorkspaceHandle) -> Option<&mut Workspace> {
        self.spaces_mut().find(|w| &w.handle == handle)
    }

    pub fn spaces_for_output(&self, output: &Output) -> impl Iterator<Item = &Workspace> {
        self.sets
            .get(output)
            .into_iter()
            .flat_map(|set| set.workspaces.iter())
    }

    pub fn spaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace> {
        Box::new(
            self.sets
                .values_mut()
                .flat_map(|set| set.workspaces.iter_mut()),
        )
    }

    pub fn set_theme(&mut self) {
        self.force_redraw();
    }

    pub fn force_redraw(&mut self) {
        for (_, s) in &mut self.sets {
            s.sticky_layer.mapped().for_each(|m| {
                m.force_redraw();
            });
            s.sticky_layer.refresh();

            for w in &mut s.workspaces {
                w.mapped().for_each(|m| {
                    m.force_redraw();
                });

                w.refresh();
                w.dirty.store(true, Ordering::Relaxed);
                w.recalculate();
            }
        }
    }

    pub fn update_autotile_behavior<'a>(
        &mut self,
        behavior: TileBehavior,
        guard: &mut WorkspaceUpdateGuard<'_, State>,
        seats: impl Iterator<Item = &'a Seat<State>>,
    ) {
        self.autotile_behavior = behavior;
        self.apply_tile_change(guard, seats);
    }

    fn apply_tile_change<'a>(
        &mut self,
        guard: &mut WorkspaceUpdateGuard<'_, State>,
        seats: impl Iterator<Item = &'a Seat<State>>,
    ) {
        let seats = seats.cloned().collect::<Vec<_>>();
        for (_, set) in &mut self.sets {
            set.tiling_enabled = self.autotile;

            if matches!(self.autotile_behavior, TileBehavior::Global) {
                // must apply change to all workspaces now
                for w in &mut set.workspaces {
                    if w.tiling_enabled == self.autotile {
                        continue;
                    }
                    for s in &seats {
                        w.toggle_tiling(s, guard);
                    }
                }
            }
        }
    }

    pub fn update_autotile<'a>(
        &mut self,
        autotile: bool,
        guard: &mut WorkspaceUpdateGuard<'_, State>,
        seats: impl Iterator<Item = &'a Seat<State>>,
    ) {
        self.autotile = autotile;
        self.apply_tile_change(guard, seats);
    }

    pub fn persist(&self, config: &Config) {
        let pinned_workspaces: Vec<PinnedWorkspace> = self
            .sets
            .values()
            .flat_map(|set| &set.workspaces)
            .flat_map(|w| w.to_pinned())
            .collect();
        let config = config.cosmic_helper.clone();
        thread::spawn(move || {
            if let Err(err) = config.set("pinned_workspaces", pinned_workspaces) {
                error!(?err, "Failed to update pinned_workspaces key");
            }
        });
    }
}

#[derive(Debug)]
pub struct InvalidWorkspaceIndex;

impl Common {
    pub fn add_output(&mut self, output: &Output) {
        let mut shell = self.shell.write();
        shell
            .workspaces
            .add_output(output, &mut self.workspace_state.update());

        if let Some(state) = shell.zoom_state.as_ref() {
            output.user_data().insert_if_missing_threadsafe(|| {
                Mutex::new(OutputZoomState::new(
                    &state.seat,
                    output,
                    1.0,
                    state.increment,
                    state.movement,
                ))
            });
        }

        std::mem::drop(shell);
        self.refresh(); // fixes indicies of any moved workspaces
    }

    pub fn remove_output(&mut self, output: &Output) {
        let mut shell = self.shell.write();
        let shell_ref = &mut *shell;
        shell_ref.workspaces.remove_output(
            output,
            shell_ref.seats.iter(),
            &mut self.workspace_state.update(),
            &self.xdg_activation_state,
        );

        std::mem::drop(shell);
        self.refresh(); // cleans up excess of workspaces and empty workspaces
    }

    pub fn update_config(&mut self) {
        let mut shell = self.shell.write();
        let shell_ref = &mut *shell;
        shell_ref.active_hint = self.config.cosmic_conf.active_hint;
        shell_ref.lunaris_theme = self.lunaris_theme.clone();
        // Feature 4-C: bump the window-header theme-generation
        // counter so every cached per-window pixmap invalidates on
        // the next frame. The rasteriser reads the current theme
        // directly via `crate::theme::lunaris_theme()`.
        crate::backend::render::window_header::bump_theme_generation();
        shell_ref.appearance_conf = self.config.cosmic_conf.appearance_settings;
        if let Some(zoom_state) = shell_ref.zoom_state.as_mut() {
            zoom_state.increment = self.config.cosmic_conf.accessibility_zoom.increment;
            zoom_state.movement = self.config.cosmic_conf.accessibility_zoom.view_moves;
            zoom_state.show_overlay = self.config.cosmic_conf.accessibility_zoom.show_overlay;

            for output in shell_ref.workspaces.sets.keys() {
                let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
                let mut output_state_ref = output_state.lock().unwrap();
                let level = output_state_ref.level;
                output_state_ref.update(level, false, zoom_state.movement, zoom_state.increment);
            }
        }

        let mut workspace_state = self.workspace_state.update();
        shell_ref.workspaces.update_config(
            &self.config,
            &mut workspace_state,
            &self.xdg_activation_state,
        );

        for mapped in shell_ref.mapped() {
            mapped.update_appearance_conf(&self.config.cosmic_conf.appearance_settings);
        }
    }

    #[profiling::function]
    pub fn refresh(&mut self) {
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < ACTIVATION_TOKEN_EXPIRE_TIME);
        {
            let mut shell = self.shell.write();
            shell.refresh(
                &self.xdg_activation_state,
                &mut self.workspace_state.update(),
            );
            for kind in shell.pending_indicator_hides.drain(..) {
                self.shell_overlay_state.send_indicator_hide(kind);
            }
            if let Some(event) = shell.pending_zoom_event.take() {
                match event {
                    ZoomProtocolEvent::Show { level, increment, movement } => {
                        self.shell_overlay_state.send_zoom_toolbar_show(level, increment, movement);
                    }
                    ZoomProtocolEvent::Update { level } => {
                        self.shell_overlay_state.send_zoom_toolbar_update(level);
                    }
                    ZoomProtocolEvent::Hide => {
                        self.shell_overlay_state.send_zoom_toolbar_hide();
                    }
                }
            }
        }
        self.popups.cleanup();
        self.shell.write().scratchpad_remove_dead();
        self.toplevel_info_state.refresh(&self.workspace_state);
        self.refresh_idle_inhibit();
        self.a11y_keyboard_monitor_state.refresh();
        self.refresh_titlebar_modes();
        self.refresh_window_headers();
        self.tick_fullscreen_reveal_timer();
    }

    /// Per-frame sync for Lunaris-rendered window headers.
    ///
    /// Covers three kinds of changes in one pass:
    /// - geometry (position / width) — from set_geometry calls
    /// - title text — from client commits
    /// - activated flag — from set_focus
    ///
    /// Implementation keeps a cache of `last_sent_payload` per
    /// `surface_id`. On each frame we compute the current payload
    /// for every SSD window and diff; only actual changes emit
    /// `window_header_update`. Windows that disappeared between
    /// frames (e.g. closed without the destroyed-handler firing
    /// yet — rare edge case) get a `window_header_hide` and their
    /// cache entry removed.
    ///
    /// One-liner cost per frame is ~one allocation per mapped
    /// window to read geometry + title; cheap relative to the
    /// render pipeline itself.
    /// Synchronously emit a `window_header_update` for exactly one
    /// window, bypassing the full per-frame diff scan. Used by
    /// `MoveGrab::motion` / `ResizeGrab::motion` (Feature 4:
    /// latency-sync) so drag updates cost O(1) instead of O(n) per
    /// pointer-motion event.
    ///
    /// The cache is still checked to avoid emitting redundant
    /// events when sub-pixel motion rounds to the same position.
    /// Silently no-ops if `mapped` is not eligible for a header.
    pub(crate) fn refresh_window_header_for(&mut self, mapped: &crate::shell::CosmicMapped) {
        let shell = self.shell.read();

        // Prefer a drag-state payload if this window is currently
        // being interactively moved: `window_header_payload`
        // resolves the window's position via the workspace, which
        // is `None` during a MoveGrab (the window is not in any
        // workspace). Without this, per-motion updates emit
        // nothing and the shell header freezes at its pre-drag
        // position.
        let mut payload: Option<crate::shell::WindowHeaderPayload> = None;
        for seat in shell.seats.iter() {
            let Some(grab_state_slot) = seat
                .user_data()
                .get::<crate::shell::grabs::SeatMoveGrabState>()
            else {
                continue;
            };
            let guard = grab_state_slot.lock().unwrap();
            if let Some(grab_state) = guard.as_ref()
                && &grab_state.window == mapped
            {
                payload = crate::shell::dragged_window_header_payload(grab_state);
                break;
            }
        }
        if payload.is_none() {
            payload = crate::shell::window_header_payload(&shell, mapped);
        }
        drop(shell);
        let Some(payload) = payload else {
            return;
        };
        let cache = &mut self.window_header_cache;
        let changed = match cache.get(&payload.surface_id) {
            None => true,
            Some(prev) => {
                prev.x != payload.x
                    || prev.y != payload.y
                    || prev.width != payload.width
                    || prev.height != payload.height
                    || prev.title != payload.title
                    || prev.activated != payload.activated
                    || prev.stack_id != payload.stack_id
            }
        };
        if !changed {
            return;
        }
        if cache.contains_key(&payload.surface_id) {
            self.shell_overlay_state.send_window_header_update(
                payload.surface_id,
                payload.x, payload.y,
                payload.width, payload.height,
                payload.title.clone(),
                payload.activated,
                payload.stack_id,
            );
        } else {
            self.shell_overlay_state.send_window_header_show(
                payload.surface_id,
                payload.x, payload.y,
                payload.width, payload.height,
                payload.title.clone(),
                payload.activated,
                true, true,
                payload.stack_id,
            );
        }
        cache.insert(
            payload.surface_id,
            crate::shell::CachedHeaderPayload::from(&payload),
        );
    }

    pub(crate) fn refresh_window_headers(&mut self) {
        use crate::shell::{should_emit_shell_header_events, window_header_payload};

        let shell = self.shell.read();
        // Build current snapshot for all eligible windows. Feature
        // 4-C switched non-stacked single-window headers over to
        // compositor rendering (`CosmicWindow::header_render_element`),
        // so we only emit shell-bound events for STACKS now; the
        // tab strip still needs the shell to paint tabs + buttons.
        let mut current: std::collections::HashMap<u32, crate::shell::WindowHeaderPayload> =
            std::collections::HashMap::new();
        for space in shell.workspaces.spaces() {
            for mapped in space.mapped() {
                if !should_emit_shell_header_events(mapped) {
                    continue;
                }
                if let Some(payload) = window_header_payload(&shell, mapped) {
                    current.insert(payload.surface_id, payload);
                }
            }
        }
        // Also scan sticky layers across all sets so sticky windows
        // get headers too.
        for set in shell.workspaces.sets.values() {
            for mapped in set.sticky_layer.mapped() {
                if !should_emit_shell_header_events(mapped) {
                    continue;
                }
                if let Some(payload) = window_header_payload(&shell, mapped) {
                    current.insert(payload.surface_id, payload);
                }
            }
        }
        // Feature 4 fix: windows inside an interactive MoveGrab are
        // NOT in any workspace for the grab's lifetime — their
        // rendering is driven from `MoveGrabState` on the seat's
        // user-data. If we skipped them here the stale-check below
        // would emit `window_header_hide` every frame during a
        // drag and the Lunaris header would visually disappear
        // until release. Poll each seat's grab state and overlay
        // a live payload so (a) the dragged window stays in
        // `current` (no hide), and (b) its position is updated
        // once per frame from the same grab state the compositor
        // is rendering from.
        // Only surface drag-state headers for STACKS — Feature 4-C
        // renders non-stacked single-window headers compositor-
        // side, so the shell doesn't need updates for those.
        for seat in shell.seats.iter() {
            let Some(grab_state_slot) = seat
                .user_data()
                .get::<crate::shell::grabs::SeatMoveGrabState>()
            else {
                continue;
            };
            let guard = grab_state_slot.lock().unwrap();
            if let Some(grab_state) = guard.as_ref()
                && should_emit_shell_header_events(&grab_state.window)
                && let Some(payload) =
                    crate::shell::dragged_window_header_payload(grab_state)
            {
                current.insert(payload.surface_id, payload);
            }
        }
        drop(shell);

        // Diff against last-sent cache.
        let cache = &mut self.window_header_cache;
        // Emit updates for changed entries.
        for (id, payload) in &current {
            let changed = match cache.get(id) {
                None => true,
                Some(prev) => {
                    prev.x != payload.x
                        || prev.y != payload.y
                        || prev.width != payload.width
                        || prev.height != payload.height
                        || prev.title != payload.title
                        || prev.activated != payload.activated
                        || prev.stack_id != payload.stack_id
                }
            };
            if changed {
                // new entries get `show` (first time), existing get
                // `update`. No-op if the shell's store already has it
                // from the map-time show call.
                if cache.contains_key(id) {
                    self.shell_overlay_state.send_window_header_update(
                        payload.surface_id,
                        payload.x, payload.y,
                        payload.width, payload.height,
                        payload.title.clone(),
                        payload.activated,
                        payload.stack_id,
                    );
                } else {
                    self.shell_overlay_state.send_window_header_show(
                        payload.surface_id,
                        payload.x, payload.y,
                        payload.width, payload.height,
                        payload.title.clone(),
                        payload.activated,
                        true, true,
                        payload.stack_id,
                    );
                }
                // Cache the clone-able fields. The cache struct is a
                // stripped-down copy of WindowHeaderPayload that
                // implements Clone — see Common::window_header_cache.
                cache.insert(*id, CachedHeaderPayload::from(payload));
            }
        }
        // Hide stale entries (window gone between frames).
        let stale: Vec<u32> = cache
            .keys()
            .filter(|id| !current.contains_key(id))
            .copied()
            .collect();
        for id in stale {
            self.shell_overlay_state.send_window_header_hide(id);
            // Feature 4 (attach): notify any bound shell
            // attachments that their target window is gone. The
            // attachments themselves are still alive until the
            // shell destroys them; they just stop tracking a
            // window.
            self.window_attach_state.unbind_window(id);
            cache.remove(&id);
        }
    }

    /// Check all surfaces with active titlebar bindings and send
    /// `mode_changed` events if the window mode has changed.
    pub fn refresh_titlebar_modes(&mut self) {
        use crate::wayland::handlers::titlebar::state_to_json;
        use smithay::reexports::wayland_server::Resource;

        // Collect surface IDs with active titlebar bindings.
        let surface_ids: Vec<u64> = self
            .titlebar_manager_state
            .active_surface_ids()
            .collect();

        if surface_ids.is_empty() {
            return;
        }

        let shell = self.shell.read();

        for sid in surface_ids {
            // Determine current window state by searching workspaces.
            let mut is_fullscreen = false;
            let mut is_tiled = false;
            let mut found = false;

            'outer: for workspace in shell.workspaces.spaces() {
                // Check fullscreen surface.
                if let Some(ref fs) = workspace.fullscreen {
                    if let Some(wl) = fs.surface.wl_surface() {
                        if wl.id().protocol_id() as u64 == sid {
                            is_fullscreen = true;
                            found = true;
                            break 'outer;
                        }
                    }
                }
                // Check tiling layer.
                for (mapped, _) in workspace.tiling_layer.mapped() {
                    for (surface, _) in mapped.windows() {
                        if let Some(wl) = surface.wl_surface() {
                            if wl.id().protocol_id() as u64 == sid {
                                is_tiled = true;
                                found = true;
                                break 'outer;
                            }
                        }
                    }
                }
                // Check floating layer.
                for mapped in workspace.floating_layer.mapped() {
                    for (surface, _) in mapped.windows() {
                        if let Some(wl) = surface.wl_surface() {
                            if wl.id().protocol_id() as u64 == sid {
                                // Floating: is_tiled = false, is_fullscreen = false.
                                found = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }

            if !found {
                continue;
            }

            let mode = if is_fullscreen {
                crate::wayland::protocols::titlebar::TitlebarMode::Fullscreen
            } else if is_tiled {
                crate::wayland::protocols::titlebar::TitlebarMode::Tiled
            } else {
                crate::wayland::protocols::titlebar::TitlebarMode::Floating
            };

            if self.titlebar_manager_state.send_mode_changed(sid, mode) {
                // Mode changed; also notify the shell.
                if let Some(tb) = self.titlebar_manager_state.get(sid) {
                    let json = state_to_json(tb);
                    self.shell_overlay_state
                        .send_window_header_content(sid as u32, json);
                }
            }
        }
    }

    /// Check the fullscreen reveal hide timer even when no pointer events
    /// arrive. Without this, a stationary pointer would leave the state
    /// machine stuck in `HidePending` forever.
    pub fn tick_fullscreen_reveal_timer(&mut self) {
        use crate::shell::fullscreen_reveal::{RevealAction, RevealPhase};

        if self.fullscreen_reveal.phase != RevealPhase::HidePending {
            return;
        }

        // Tick with the same pointer_y that caused the leave (outside
        // titlebar). The exact value does not matter as long as it is
        // above the titlebar threshold (36px).
        let prev_sid = self.fullscreen_reveal.surface_id;
        let action = self.fullscreen_reveal.update(100.0, true, prev_sid);
        if action == RevealAction::Hide {
            let hide_sid = if prev_sid != 0 { prev_sid } else { 0 };
            self.shell_overlay_state
                .send_fullscreen_titlebar_hide(hide_sid);
        }
    }

    pub fn refresh_idle_inhibit(&mut self) {
        self.idle_inhibiting_surfaces.retain(|s| s.alive());

        let is_inhibited = self.idle_inhibiting_surfaces.iter().any(|surface| {
            with_states(surface, |states| {
                surface_primary_scanout_output(surface, states).is_some()
            })
        });
        self.idle_notifier_state.set_is_inhibited(is_inhibited);
    }

    #[profiling::function]
    pub fn on_commit(&mut self, surface: &WlSurface) {
        {
            let shell = self.shell.read();

            for seat in shell.seats.iter() {
                if let Some(move_grab) = seat.user_data().get::<SeatMoveGrabState>()
                    && let Some(grab_state) = move_grab.lock().unwrap().as_ref()
                {
                    let mapped = grab_state.element();
                    if mapped.active_window().wl_surface().as_deref() == Some(surface) {
                        mapped.on_commit(surface);
                    }
                }

                data_device::on_commit(surface, seat);
            }

            let is_cursor_image = shell.seats.iter().any(|seat| {
                    matches!(seat.cursor_image_status(), CursorImageStatus::Surface(ref cursor_surface) if cursor_surface == surface)
            });

            if is_cursor_image {
                with_states(surface, |states| {
                    let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                    if let Some(mut cursor_image_attributes) =
                        cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                    {
                        let buffer_delta = states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take();
                        if let Some(buffer_delta) = buffer_delta {
                            cursor_image_attributes.hotspot -= buffer_delta;
                        }
                    }
                });
            }

            if let Some(mapped) = shell.element_for_surface(surface) {
                mapped.on_commit(surface);
            }
            if let Some(surface) = shell
                .workspaces
                .spaces()
                .find_map(|w| w.get_fullscreen().filter(|s| *s == surface))
            {
                surface.on_commit()
            };
        }
        self.popups.commit(surface);
    }
}

/// Decides whether the given window should receive a Lunaris-rendered
/// header via the `lunaris-shell-overlay` `window_header_*` protocol.
///
/// Four inclusion criteria (all must be true):
///
/// 1. Wayland surface — X11 clients have their own decoration paradigm
///    (MOTIF_WM_HINTS with inverted semantics — see decoration-spec
///    discussion); excluded from v1 to avoid surprises.
/// 2. Not a stack — stacks carry their own tab-bar which already IS
///    the decoration.
/// 3. Client requested SSD — `has_ssd == true` means the client either
///    explicitly asked for server-side via xdg/kde-decoration, or it
///    bound the protocol without committing a mode. Clients that draw
///    their own CSD (GTK, Qt with default settings, Firefox, most
///    Electron apps) never hit this branch.
/// 4. Not fullscreen — fullscreen content shouldn't be pushed down by
///    a 36px header.
/// Global mirror of `Shell::tiled_headers_enabled` so the render
/// decision can be made from anywhere without threading a `&Shell`
/// through the call chain. Same pattern as
/// `window_header::theme_generation()`. Updated only via
/// `set_tiled_headers_global` from the canonical
/// `Shell::set_tiled_headers_enabled` method.
static TILED_HEADERS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Read the global tiled-headers toggle. Called from
/// `should_render_window_header` and `CosmicWindowInternal::has_ssd`.
pub fn tiled_headers_enabled_global() -> bool {
    TILED_HEADERS_ENABLED.load(Ordering::Relaxed)
}

/// Set the global mirror. Use `Shell::set_tiled_headers_enabled`
/// from outside; this is the low-level setter that ALSO needs to
/// fire from there so render decisions and instance state stay
/// aligned.
pub(crate) fn set_tiled_headers_global(v: bool) {
    TILED_HEADERS_ENABLED.store(v, Ordering::Relaxed);
}

/// General eligibility check — is this window "dressable" at all?
/// Used by both the compositor-rendered header path (Feature 4-C,
/// single non-stacked windows) and the shell-rendered stack header
/// path. Two callers share this so policy stays in one place.
///
/// Order of checks is **load-bearing** (see Tiled-Headers Toggle
/// edge-case analysis):
///   1. Stacks always render — toggle never applies, tab-bar is
///      functional UI.
///   2. Fullscreen never renders — covers entire output, no chrome.
///   3. Tiled + toggle-off never renders — user opted out for
///      single tiled windows. Floating windows in the same
///      workspace are unaffected because `is_tiled()` is per
///      window, not per workspace.
///   4. Otherwise: client-side-decoration check (Wayland) or
///      X11 heuristic.
pub fn should_render_window_header(mapped: &CosmicMapped) -> bool {
    if mapped.is_stack() {
        return true;
    }
    let surface = mapped.active_window();
    let is_fullscreen = surface.is_fullscreen(false);
    // Workspace-level placement flag, NOT the xdg-toplevel tiled state.
    // The latter is set on floating windows too when
    // `clip_floating_windows` is enabled (rectangular clipping intent),
    // so reading it here would suppress floating SSD windows' headers
    // whenever the user toggles `tiled_headers` off.
    let is_tiled = mapped.is_in_tiling_layer();
    let tiled_enabled = tiled_headers_enabled_global();

    let csd_branch_eligible = match surface.0.underlying_surface() {
        smithay::desktop::WindowSurface::Wayland(_) => !surface.is_decorated(false),
        smithay::desktop::WindowSurface::X11(x11) => x11_should_render_header(x11),
    };

    header_eligibility_pure(HeaderEligibilityInputs {
        is_stack: false, // already short-circuited
        is_fullscreen,
        is_tiled,
        tiled_headers_enabled: tiled_enabled,
        csd_branch_eligible,
    })
}

/// Inputs describing one window's state for header-eligibility.
/// Carved out so the policy itself is a pure function with a
/// testable truth table — no need to mock CosmicMapped/CosmicSurface.
#[derive(Debug, Clone, Copy)]
pub struct HeaderEligibilityInputs {
    /// `true` for `CosmicMapped::is_stack()`. When set, the policy
    /// always returns `true` regardless of the other inputs (stacks
    /// own their own tab-bar UI which is functional, not decoration).
    pub is_stack: bool,
    /// `true` if the window is currently fullscreen — never
    /// renders a header (chrome would obscure content the user is
    /// fullscreen-viewing).
    pub is_fullscreen: bool,
    /// `true` if the window is currently placed in the workspace's
    /// tiling layer. False for floating. **Not** the xdg-toplevel
    /// tiled state — that flag is also set on floating windows when
    /// `clip_floating_windows` is enabled (clipping intent), so
    /// reading it here would suppress floating SSD windows' headers
    /// when the user toggles `tiled_headers` off. Use
    /// `CosmicMapped::is_in_tiling_layer()` to populate this.
    pub is_tiled: bool,
    /// Global toggle: `true` means tiled windows DO render a header,
    /// `false` means they don't. Floating windows ignore this.
    pub tiled_headers_enabled: bool,
    /// Result of the per-protocol CSD check: Wayland xdg-decoration
    /// ServerSide / unset → true; X11 motif-hints + heuristic → true
    /// when window is undecorated by the client and eligible for
    /// Lunaris chrome.
    pub csd_branch_eligible: bool,
}

/// Pure render-eligibility policy. **Order is load-bearing**:
///   1. Stacks → always true.
///   2. Fullscreen → always false.
///   3. Tiled + toggle off → false (per-window, NOT per-workspace).
///   4. Otherwise: client-side-decoration branch.
pub fn header_eligibility_pure(i: HeaderEligibilityInputs) -> bool {
    if i.is_stack {
        return true;
    }
    if i.is_fullscreen {
        return false;
    }
    if i.is_tiled && !i.tiled_headers_enabled {
        return false;
    }
    i.csd_branch_eligible
}

/// Should this window's header events (`window_header_show` /
/// `window_header_update` / `window_header_hide`) be sent to the
/// desktop-shell process for Svelte rendering? Since Feature 4-C
/// moves single-window headers into the compositor, the shell only
/// needs events for **stacks** — its WindowHeader.svelte is still
/// the thing drawing the tab strip integrated with the window
/// controls (Feature 3). Non-stacked single-window headers are
/// now fully handled by `CosmicWindow::header_render_element`
/// during the GL render pass and the shell never sees them.
pub fn should_emit_shell_header_events(mapped: &CosmicMapped) -> bool {
    mapped.is_stack() && should_render_window_header(mapped)
}

/// Hybrid X11 header-eligibility heuristic. See blueprint Limitation 1
/// analysis — ranks a window against four rules, all must pass:
///
/// 1. Not override-redirect, not popup, not a transient of another
///    window. These are menus, tooltips, DnD feedback, modal dialogs
///    — they get their own handling upstream.
/// 2. `_NET_WM_WINDOW_TYPE` is either `Normal` or unset. Excludes
///    Dialog, Utility, Toolbar, Menu, Dock, Dnd, Splash etc.
/// 3. Content geometry is at least 200×100 logical px. Rules out
///    tiny popups that slipped through the earlier filters.
/// 4. Smithay's `X11Surface::is_decorated()` reports `false` —
///    i.e., the app has NOT set Motif hints to `MWM_DECOR_NONE`.
///    Unset Motif hints default to "please decorate" (the Motif
///    spec default of `MWM_DECOR_ALL`), which is exactly the case
///    where a Lunaris header is useful for legacy X11 apps that
///    have no native chrome. Apps that set `MWM_DECOR_NONE`
///    (modern GTK3+, Qt5 X11, Steam) draw their own title bar and
///    MUST NOT get a second one from us.
///
/// The semantics of `is_decorated()` on X11 in the vendored
/// Smithay (see `/src/xwayland/xwm/surface.rs:545`) match Wayland's
/// — returns `true` for self-decorated clients — so the call above
/// can be uniform across both branches at the higher level.
fn x11_should_render_header(x11: &smithay::xwayland::X11Surface) -> bool {
    use smithay::xwayland::xwm::WmWindowType;

    if x11.is_override_redirect() {
        tracing::debug!(
            "X11-DEBUG window_id={} skipped: override_redirect",
            x11.window_id()
        );
        return false;
    }
    if x11.is_popup() {
        tracing::debug!(
            "X11-DEBUG window_id={} skipped: popup",
            x11.window_id()
        );
        return false;
    }
    if x11.is_transient_for().is_some() {
        tracing::debug!(
            "X11-DEBUG window_id={} skipped: transient_for present",
            x11.window_id()
        );
        return false;
    }

    match x11.window_type() {
        None | Some(WmWindowType::Normal) => {}
        Some(other) => {
            tracing::debug!(
                "X11-DEBUG window_id={} skipped: window_type={:?}",
                x11.window_id(), other
            );
            return false;
        }
    }

    let geo = x11.geometry();
    if geo.size.w < 200 || geo.size.h < 100 {
        tracing::debug!(
            "X11-DEBUG window_id={} skipped: size {}x{} below 200x100 gate",
            x11.window_id(), geo.size.w, geo.size.h
        );
        return false;
    }

    if x11.is_decorated() {
        tracing::debug!(
            "X11-DEBUG window_id={} skipped: is_decorated()=true (app set MWM_DECOR_NONE)",
            x11.window_id()
        );
        return false;
    }

    tracing::debug!(
        "X11-DEBUG window_id={} eligible: size={}x{} type=Normal no_motif_none",
        x11.window_id(), geo.size.w, geo.size.h
    );
    true
}

/// Derive the `window_header_show/update` payload for a window.
/// Returns `None` if the window isn't eligible (see
/// `should_render_window_header`) OR geometry can't be resolved yet
/// (e.g. called before the workspace has placed the window).
///
/// Coordinates are GLOBAL: `output.geometry().loc + workspace_local_loc`.
/// Width is taken from the window's geometry; height is always
/// `SSD_HEIGHT` (36px, matches the compositor's reserved space).
pub struct WindowHeaderPayload {
    pub surface_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub title: String,
    pub activated: bool,
    /// Non-zero if this header belongs to a CosmicStack. The shell
    /// uses it to correlate with the `tab_added/tab_activated/...`
    /// stream and renders tabs inside the header. See Feature 3
    /// (integrated stack header) for the design.
    pub stack_id: u32,
}

/// Per-frame cache entry for the window-header diff loop. Same data
/// as `WindowHeaderPayload` but without the surface_id (it's the
/// HashMap key) and deriving `Clone` so we can snapshot the last
/// sent values.
#[derive(Clone, Debug)]
pub(crate) struct CachedHeaderPayload {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub title: String,
    pub activated: bool,
    pub stack_id: u32,
}

impl From<&WindowHeaderPayload> for CachedHeaderPayload {
    fn from(p: &WindowHeaderPayload) -> Self {
        Self {
            x: p.x, y: p.y,
            width: p.width, height: p.height,
            title: p.title.clone(),
            activated: p.activated,
            stack_id: p.stack_id,
        }
    }
}

/// Top bit used as a namespace marker for X11-originated surface ids
/// in the `lunaris-shell-overlay::window_header_*` protocol. Wayland
/// protocol ids (client-allocated, start at 2 and increment) never
/// have this bit set in practice; X11 XIDs are 29-bit on all real X
/// servers so masking with `0x7FFF_FFFF` doesn't lose information
/// and shifting `0x8000_0000` in gives us a clean split.
///
/// The shell treats this as an opaque `u32` — it just echoes it back
/// on `window_header_action`. Only the compositor-side resolver
/// (`window_header_action` handler) needs to interpret the top bit to
/// look the window up in the right table.
pub const HEADER_ID_X11_NAMESPACE: u32 = 0x8000_0000;

/// Derive the protocol surface-id for a mapped window's header.
/// Stable across frames for the same window — both the refresh diff
/// and the action resolver rely on this invariant.
pub fn window_header_surface_id(mapped: &CosmicMapped) -> Option<u32> {
    use smithay::reexports::wayland_server::Resource;

    let surface = mapped.active_window();
    match surface.0.underlying_surface() {
        smithay::desktop::WindowSurface::Wayland(_) => {
            // Wayland: use the wl_surface protocol id with the top
            // bit forced clear. In practice client-allocated ids
            // never have it set; the mask is a belt-and-braces.
            let wl = surface.wl_surface()?;
            Some(wl.id().protocol_id() & !HEADER_ID_X11_NAMESPACE)
        }
        smithay::desktop::WindowSurface::X11(x11) => {
            // X11: XID (< 2^29 in practice) with the top bit set to
            // declare the namespace. Keeps the id stable for the
            // window's full lifetime and lets the action resolver
            // pick the right surface family on lookup.
            Some(x11.window_id() | HEADER_ID_X11_NAMESPACE)
        }
    }
}

/// Payload for a window whose position is currently driven by an
/// interactive `MoveGrab` rather than a workspace layout. The
/// per-frame refresh calls this for every active seat's move-grab
/// state so the shell gets live header updates AND the window
/// stays registered in `current` (which prevents the stale check
/// from emitting `window_header_hide` mid-drag and making the
/// header visually disappear — the exact symptom we hit).
///
/// Coordinates come directly from the grab state: `location +
/// window_offset` is already the global-logical origin of the
/// window as rendered (see `MoveGrabState::render`). No workspace
/// lookup, no `space_for()` call — during a drag the window is
/// not in any workspace.
pub fn dragged_window_header_payload(
    grab_state: &crate::shell::grabs::MoveGrabState,
) -> Option<WindowHeaderPayload> {
    let mapped = &grab_state.window;
    if !should_render_window_header(mapped) {
        return None;
    }
    let surface_id = window_header_surface_id(mapped)?;

    let origin = grab_state.location.to_i32_round() + grab_state.window_offset;
    let size = mapped.geometry().size;

    let surface = mapped.active_window();
    let stack_id = mapped
        .stack_ref()
        .map(|stack| stack.stack_id())
        .unwrap_or(0);

    Some(WindowHeaderPayload {
        surface_id,
        x: origin.x,
        y: origin.y,
        width: size.w,
        height: element::window::SSD_HEIGHT,
        title: surface.title(),
        activated: surface.is_activated(false),
        stack_id,
    })
}

pub fn window_header_payload(
    shell: &Shell,
    mapped: &CosmicMapped,
) -> Option<WindowHeaderPayload> {
    if !should_render_window_header(mapped) {
        return None;
    }
    let surface = mapped.active_window();
    let surface_id = window_header_surface_id(mapped)?;

    let workspace = shell.space_for(mapped)?;
    let local_rect = workspace.element_geometry(mapped)?;
    let output_geo = workspace.output().geometry();

    // STACK-DEBUG: if this is a stack, tack its stack_id onto the
    // payload so the shell can render tabs inside the header. 0 for
    // non-stacks.
    let stack_id = mapped
        .stack_ref()
        .map(|stack| stack.stack_id())
        .unwrap_or(0);

    Some(WindowHeaderPayload {
        surface_id,
        x: output_geo.loc.x + local_rect.loc.x,
        // Header sits directly above the window content. The
        // compositor already reserves SSD_HEIGHT pixels via
        // has_ssd → window.rs adds SSD_HEIGHT to total height.
        // The header lives in that reserved top strip.
        y: output_geo.loc.y + local_rect.loc.y,
        width: local_rect.size.w,
        height: element::window::SSD_HEIGHT,
        title: surface.title(),
        activated: surface.is_activated(false),
        stack_id,
    })
}

impl Shell {
    pub fn new(config: &Config) -> Self {
        let tiling_exceptions = layout::TilingExceptions::new(config.tiling_exceptions.iter());

        Shell {
            workspaces: Workspaces::new(config),
            seats: Seats::new(),

            pending_windows: Vec::new(),
            pending_layers: Vec::new(),
            pending_activations: HashMap::new(),
            override_redirect_windows: Vec::new(),
            session_lock: None,
            previous_workspace_idx: None,
            xwayland_keyboard_grab: None,

            active_hint: config.cosmic_conf.active_hint,
            tiled_headers_enabled: {
                // Seed the global atomic mirror at construction so
                // the very first `should_render_window_header`
                // callsite sees the same value. Otherwise the
                // first frame races: field set, atomic still
                // default-false. set_tiled_headers_global is the
                // canonical writer.
                set_tiled_headers_global(config.layout.tiled_headers);
                config.layout.tiled_headers
            },
            lunaris_theme: lunaris_theme::LunarisTheme::load(),
            overview_mode: OverviewMode::None,
            swap_indicator: None,
            resize_mode: ResizeMode::None,
            resize_state: None,
            resize_indicator: None,
            pending_indicator_hides: Vec::new(),
            pending_zoom_event: None,
            appearance_conf: config.cosmic_conf.appearance_settings,
            zoom_state: None,
            tiling_exceptions,
            window_rules: config.layout.window_rules.clone(),
            scratchpad: ScratchpadState::default(),

            #[cfg(feature = "debug")]
            debug_active: false,
        }
    }

    pub fn activate(
        &mut self,
        output: &Output,
        idx: usize,
        workspace_delta: WorkspaceDelta,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) -> Result<Point<i32, Global>, InvalidWorkspaceIndex> {
        tracing::info!(
            "Shell::activate: output={} idx={}",
            output.name(),
            idx,
        );
        match &mut self.workspaces.mode {
            WorkspaceMode::OutputBound => {
                if let Some(set) = self.workspaces.sets.get_mut(output) {
                    if matches!(
                        self.overview_mode.active_trigger(),
                        Some(Trigger::Pointer(_) | Trigger::Touch(_))
                    ) {
                        set.workspaces[set.active].tiling_layer.cleanup_drag();
                    }
                    set.activate(idx, workspace_delta, workspace_state)?;

                    let output_geo = output.geometry();
                    Ok(
                        output_geo.loc
                            + Point::from((output_geo.size.w / 2, output_geo.size.h / 2)),
                    )
                } else {
                    Err(InvalidWorkspaceIndex)
                }
            }
            WorkspaceMode::Global => {
                for set in self.workspaces.sets.values_mut() {
                    set.activate(idx, workspace_delta, workspace_state)?;
                }
                let output_geo = output.geometry();
                Ok(output_geo.loc + Point::from((output_geo.size.w / 2, output_geo.size.h / 2)))
            }
        }
    }

    pub fn update_workspace_delta(&mut self, output: &Output, delta: f64, forward: bool) {
        match &mut self.workspaces.mode {
            WorkspaceMode::OutputBound => {
                if let Some(set) = self.workspaces.sets.get_mut(output) {
                    set.update_workspace_delta(delta, forward);
                }
            }
            WorkspaceMode::Global => {
                for set in self.workspaces.sets.values_mut() {
                    set.update_workspace_delta(delta, forward);
                }
            }
        }
    }

    pub fn end_workspace_swipe(
        &mut self,
        output: &Output,
        velocity: f64,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) -> Result<Point<i32, Global>, InvalidWorkspaceIndex> {
        match &mut self.workspaces.mode {
            WorkspaceMode::OutputBound => {
                if let Some(set) = self.workspaces.sets.get_mut(output) {
                    if matches!(
                        self.overview_mode.active_trigger(),
                        Some(Trigger::Pointer(_) | Trigger::Touch(_))
                    ) {
                        set.workspaces[set.active].tiling_layer.cleanup_drag();
                    }
                    if let Some((
                        _,
                        WorkspaceDelta::Gesture {
                            percentage: delta,
                            forward,
                        },
                    )) = set.previously_active
                    {
                        if (velocity > 0.0 && velocity.abs() >= GESTURE_VELOCITY_THRESHOLD)
                            || (velocity.abs() < GESTURE_VELOCITY_THRESHOLD
                                && delta.abs() > GESTURE_POSITION_THRESHOLD)
                        {
                            set.activate(
                                set.active,
                                WorkspaceDelta::new_gesture_end(
                                    delta.abs(),
                                    velocity.abs(),
                                    forward,
                                ),
                                workspace_state,
                            )?;
                        } else {
                            set.activate_previous(
                                WorkspaceDelta::new_gesture_end(
                                    1.0 - delta.abs(),
                                    velocity.abs(),
                                    !forward,
                                ),
                                workspace_state,
                            )?;
                        }
                    }

                    let output_geo = output.geometry();
                    Ok(
                        output_geo.loc
                            + Point::from((output_geo.size.w / 2, output_geo.size.h / 2)),
                    )
                } else {
                    Err(InvalidWorkspaceIndex)
                }
            }
            WorkspaceMode::Global => {
                for set in self.workspaces.sets.values_mut() {
                    if let Some((
                        _,
                        WorkspaceDelta::Gesture {
                            percentage: delta,
                            forward,
                        },
                    )) = set.previously_active
                    {
                        if (velocity > 0.0 && velocity.abs() >= GESTURE_VELOCITY_THRESHOLD)
                            || (velocity.abs() < GESTURE_VELOCITY_THRESHOLD
                                && delta.abs() > GESTURE_POSITION_THRESHOLD)
                        {
                            set.activate(
                                set.active,
                                WorkspaceDelta::new_gesture_end(
                                    delta.abs(),
                                    velocity.abs(),
                                    forward,
                                ),
                                workspace_state,
                            )?;
                        } else {
                            set.activate_previous(
                                WorkspaceDelta::new_gesture_end(
                                    1.0 - delta.abs(),
                                    velocity.abs(),
                                    !forward,
                                ),
                                workspace_state,
                            )?;
                        }
                    }
                }
                Err(InvalidWorkspaceIndex)
            }
        }
    }

    pub fn active_space(&self, output: &Output) -> Option<&Workspace> {
        self.workspaces.active(output).map(|(_, active)| active)
    }

    pub fn active_space_mut(&mut self, output: &Output) -> Option<&mut Workspace> {
        self.workspaces.active_mut(output)
    }

    /// get the parent output of the window which has keyboard focus (for a given seat)
    pub fn get_output_for_focus(&self, seat: &Seat<State>) -> Option<Output> {
        let mut focus_target = seat.get_keyboard().unwrap().current_focus()?;

        if let KeyboardFocusTarget::Popup(popup) = &focus_target {
            let new_target = match popup {
                PopupKind::Xdg(popup) => {
                    if let Some(parent) = popup.get_parent_surface() {
                        self.element_for_surface(&parent).cloned()
                    } else {
                        None
                    }
                }
                PopupKind::InputMethod(popup) => {
                    if let Some(parent) = popup.get_parent() {
                        self.element_for_surface(&parent.surface).cloned()
                    } else {
                        None
                    }
                }
            }?;

            focus_target = KeyboardFocusTarget::Element(new_target);
        }

        match focus_target {
            KeyboardFocusTarget::Element(elem) => {
                if seat
                    .user_data()
                    .get::<SeatMoveGrabState>()
                    .is_some_and(|state| {
                        state
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_some_and(|state| state.element() == elem)
                    })
                {
                    return Some(seat.active_output());
                }

                self.outputs()
                    .find(|output| {
                        let is_sticky = self
                            .workspaces
                            .sets
                            .get(*output)
                            .unwrap()
                            .sticky_layer
                            .mapped()
                            .any(|m| m == &elem);

                        let workspace = self.active_space(output).unwrap();
                        let is_mapped = workspace.mapped().any(|m| m == &elem);

                        is_sticky || is_mapped
                    })
                    .cloned()
            }
            KeyboardFocusTarget::Fullscreen(elem) => self
                .outputs()
                .find(|output| {
                    let workspace = self.active_space(output).unwrap();
                    workspace.get_fullscreen() == Some(&elem)
                })
                .cloned(),
            KeyboardFocusTarget::Group(WindowGroup { node, .. }) => self
                .outputs()
                .find(|output| {
                    self.workspaces
                        .active(output)
                        .unwrap()
                        .1
                        .tiling_layer
                        .has_node(&node)
                })
                .cloned(),
            KeyboardFocusTarget::LayerSurface(layer) => self
                .outputs()
                .find(|output| layer_map_for_output(output).layers().any(|l| l == &layer))
                .cloned(),
            KeyboardFocusTarget::LockSurface(surface) => self
                .session_lock
                .as_ref()?
                .surfaces
                .iter()
                .find_map(|(output, s)| (s == &surface).then_some(output))
                .cloned(),
            KeyboardFocusTarget::Popup(_) => unreachable!(),
        }
    }

    /// Coerce a keyboard focus target into a CosmicMapped element. This is useful when performing window specific
    /// actions, such as closing a window
    pub fn focused_element(&self, focus_target: &KeyboardFocusTarget) -> Option<CosmicMapped> {
        match focus_target {
            KeyboardFocusTarget::Element(window) => Some(window).cloned(),
            KeyboardFocusTarget::Popup(PopupKind::Xdg(popup)) => {
                if let Some(parent) = popup.get_parent_surface() {
                    self.element_for_surface(&parent).cloned()
                } else {
                    None
                }
            }
            KeyboardFocusTarget::Popup(PopupKind::InputMethod(popup)) => {
                if let Some(parent) = popup.get_parent() {
                    self.element_for_surface(&parent.surface).cloned()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Close the focused keyboard focus target
    pub fn close_focused(&self, focus_target: &KeyboardFocusTarget) {
        match focus_target {
            KeyboardFocusTarget::Group(_group) => {
                //TODO: decide if we want close actions to apply to groups
            }
            KeyboardFocusTarget::Fullscreen(surface) => {
                surface.close();
            }
            x => {
                if let Some(mapped) = self.focused_element(x) {
                    mapped.send_close();
                }
            }
        }
    }

    pub fn refresh_active_space(&mut self, output: &Output) {
        if let Some(w) = self.workspaces.active_mut(output) {
            w.refresh()
        }
    }

    pub fn visible_output_for_surface(&self, surface: &WlSurface) -> Option<&Output> {
        if let Some(session_lock) = &self.session_lock {
            return session_lock
                .surfaces
                .iter()
                .find(|(_, v)| v.wl_surface() == surface)
                .map(|(k, _)| k);
        }

        self.outputs()
            // layer map surface?
            .find(|o| {
                let map = layer_map_for_output(o);
                map.layer_for_surface(surface, WindowSurfaceType::ALL)
                    .is_some()
            })
            // pending layer map surface?
            .or_else(|| {
                self.pending_layers.iter().find_map(|pending| {
                    let mut found = false;
                    pending.surface.with_surfaces(|s, _| {
                        if s == surface {
                            found = true;
                        }
                    });
                    found.then_some(&pending.output)
                })
            })
            // override redirect window?
            .or_else(|| {
                self.outputs().find(|o| {
                    self.override_redirect_windows.iter().any(|or| {
                        if or.wl_surface().as_ref() == Some(surface) {
                            or.geometry()
                                .as_global()
                                .intersection(o.geometry())
                                .is_some()
                        } else {
                            false
                        }
                    })
                })
            })
            // sticky window ?
            .or_else(|| {
                self.outputs().find(|o| {
                    self.workspaces.sets[*o]
                        .sticky_layer
                        .mapped()
                        .any(|e| e.has_surface(surface, WindowSurfaceType::ALL))
                })
            })
            // normal window?
            .or_else(|| {
                self.outputs().find(|o| {
                    let workspace = self.active_space(o).unwrap();

                    workspace
                        .get_fullscreen()
                        .is_some_and(|s| s.has_surface(surface, WindowSurfaceType::ALL))
                        || workspace
                            .mapped()
                            .any(|e| e.has_surface(surface, WindowSurfaceType::ALL))
                })
            })
            // cursor and drag surfaces
            .or_else(|| {
                self.outputs().find(|o| {
                    self.seats
                        .iter()
                        .filter(|seat| seat.active_output() == **o)
                        .any(|seat| {
                            let cursor_status = seat.cursor_image_status();
                            if let CursorImageStatus::Surface(s) = cursor_status
                                && s == *surface
                            {
                                return true;
                            }

                            if let Some(move_grab) = seat.user_data().get::<SeatMoveGrabState>()
                                && let Some(grab_state) = move_grab.lock().unwrap().as_ref()
                            {
                                for (window, _) in grab_state.element().windows() {
                                    let mut matches = false;
                                    window.0.with_surfaces(|s, _| {
                                        matches |= s == surface;
                                    });
                                    if matches {
                                        return true;
                                    }
                                }
                            }

                            get_dnd_icon(seat).is_some_and(|icon| icon.surface == *surface)
                        })
                })
            })
    }

    pub fn workspace_for_surface(&self, surface: &WlSurface) -> Option<(WorkspaceHandle, Output)> {
        match self.outputs().find(|o| {
            let map = layer_map_for_output(o);
            map.layer_for_surface(surface, WindowSurfaceType::ALL)
                .is_some()
        }) {
            Some(output) => self
                .workspaces
                .spaces()
                .find(move |workspace| workspace.output() == output)
                .map(|w| (w.handle, output.clone())),
            None => self
                .workspaces
                .spaces()
                .find(|w| {
                    w.get_fullscreen()
                        .is_some_and(|s| s.has_surface(surface, WindowSurfaceType::ALL))
                        || w.mapped()
                            .any(|e| e.has_surface(surface, WindowSurfaceType::ALL))
                        || w.minimized_windows.iter().any(|m| {
                            m.mapped()
                                .is_some_and(|m| m.has_surface(surface, WindowSurfaceType::ALL))
                        })
                })
                .map(|w| (w.handle, w.output().clone())),
        }
    }

    pub fn element_for_surface<S>(&self, surface: &S) -> Option<&CosmicMapped>
    where
        CosmicSurface: PartialEq<S>,
    {
        self.workspaces.sets.values().find_map(|set| {
            set.minimized_windows
                .iter()
                .find(|w| w.windows().any(|s| &s == surface))
                .and_then(|w| w.mapped())
                .or_else(|| {
                    set.sticky_layer
                        .mapped()
                        .find(|w| w.windows().any(|(s, _)| &s == surface))
                })
                .or_else(|| {
                    set.workspaces
                        .iter()
                        .find_map(|w| w.element_for_surface(surface))
                })
        })
    }

    pub fn is_surface_mapped<S>(&self, surface: &S) -> bool
    where
        CosmicSurface: PartialEq<S>,
    {
        self.workspaces.sets.values().any(|set| {
            set.minimized_windows
                .iter()
                .any(|w| w.windows().any(|s| &s == surface))
                || set
                    .sticky_layer
                    .mapped()
                    .any(|m| m.windows().any(|(s, _)| &s == surface))
                || set.workspaces.iter().any(|w| {
                    w.get_fullscreen().is_some_and(|s| s == surface)
                        || w.minimized_windows
                            .iter()
                            .any(|m| m.windows().any(|s| &s == surface))
                        || w.floating_layer
                            .mapped()
                            .any(|m| m.windows().any(|(s, _)| &s == surface))
                        || w.tiling_layer
                            .mapped()
                            .any(|(m, _)| m.windows().any(|(s, _)| &s == surface))
                })
        })
    }

    pub fn space_for(&self, mapped: &CosmicMapped) -> Option<&Workspace> {
        self.workspaces.spaces().find(|workspace| {
            workspace.mapped().any(|m| m == mapped)
                || workspace
                    .minimized_windows
                    .iter()
                    .any(|m| m.mapped() == Some(mapped))
        })
    }

    pub fn space_for_mut(&mut self, mapped: &CosmicMapped) -> Option<&mut Workspace> {
        self.workspaces.spaces_mut().find(|workspace| {
            workspace.mapped().any(|m| m == mapped)
                || workspace
                    .minimized_windows
                    .iter()
                    .any(|m| m.mapped() == Some(mapped))
        })
    }

    pub fn outputs(&self) -> impl DoubleEndedIterator<Item = &Output> {
        self.workspaces.sets.keys()
    }

    pub fn next_output(&self, current_output: &Output, direction: Direction) -> Option<&Output> {
        let current_output_geo = current_output.geometry();
        self.outputs()
            .filter(|o| *o != current_output)
            .filter(|o| {
                let geo = o.geometry();
                match direction {
                    Direction::Left | Direction::Right => {
                        geo.loc.y < current_output_geo.loc.y + current_output_geo.size.h
                            && geo.loc.y + geo.size.h > current_output_geo.loc.y
                    }
                    Direction::Up | Direction::Down => {
                        geo.loc.x < current_output_geo.loc.x + current_output_geo.size.w
                            && geo.loc.x + geo.size.w > current_output_geo.loc.x
                    }
                }
            })
            .filter_map(|o| {
                let origin = o.geometry().loc;
                let res = match direction {
                    Direction::Up => current_output_geo.loc.y - origin.y,
                    Direction::Down => origin.y - current_output_geo.loc.y,
                    Direction::Left => current_output_geo.loc.x - origin.x,
                    Direction::Right => origin.x - current_output_geo.loc.x,
                };
                if res > 0 { Some((o, res)) } else { None }
            })
            .min_by_key(|(_, res)| *res)
            .map(|(o, _)| o)
    }

    pub fn builtin_output(&self) -> Option<&Output> {
        self.outputs().find(|output| output.is_internal())
    }

    pub fn global_space(&self) -> Rectangle<i32, Global> {
        self.outputs()
            .fold(
                Option::<Rectangle<i32, Global>>::None,
                |maybe_geo, output| match maybe_geo {
                    Some(rect) => Some(rect.merge(output.geometry())),
                    None => Some(output.geometry()),
                },
            )
            .unwrap_or_else(Rectangle::default)
    }

    pub fn animations_going(&self) -> bool {
        self.workspaces.sets.values().any(|set| {
            set.previously_active
                .as_ref()
                .is_some_and(|(_, delta)| delta.is_animating())
                || set.sticky_layer.animations_going()
        }) || !matches!(
            self.overview_mode,
            OverviewMode::None | OverviewMode::Active(_)
        ) || !matches!(
            self.resize_mode,
            ResizeMode::None | ResizeMode::Active(_, _)
        ) || self
            .workspaces
            .spaces()
            .any(|workspace| workspace.animations_going())
            || self.zoom_state.as_ref().is_some_and(|_| {
                self.outputs().any(|o| {
                    o.user_data()
                        .get::<Mutex<OutputZoomState>>()
                        .is_some_and(|state| state.lock().unwrap().is_animating())
                })
            })
    }

    pub fn update_animations(&mut self) -> HashMap<ClientId, Client> {
        let mut clients = HashMap::new();
        for set in self.workspaces.sets.values_mut() {
            set.sticky_layer.update_animation_state();
        }
        for workspace in self.workspaces.spaces_mut() {
            clients.extend(workspace.update_animations());
        }
        clients
    }

    pub fn set_overview_mode(
        &mut self,
        enabled: Option<Trigger>,
        evlh: LoopHandle<'static, crate::state::State>,
    ) {
        if let Some(trigger) = enabled {
            if !matches!(
                self.overview_mode,
                OverviewMode::Started(_, _) | OverviewMode::Active(_)
            ) {
                if matches!(trigger, Trigger::KeyboardSwap(_, _)) {
                    self.swap_indicator = Some(swap_indicator(evlh.clone()));
                    evlh.insert_idle(|state| {
                        state.common.shell_overlay_state.send_indicator_show(
                            2, 0, 0, String::new(), String::new(),
                        );
                    });
                }
                self.overview_mode = OverviewMode::Started(trigger, Instant::now());
            }
        } else if matches!(
            self.overview_mode,
            OverviewMode::Started(_, _) | OverviewMode::Active(_)
        ) {
            let (reverse_duration, trigger) =
                if let OverviewMode::Started(trigger, start) = self.overview_mode.clone() {
                    (
                        ANIMATION_DURATION
                            - Instant::now().duration_since(start).min(ANIMATION_DURATION),
                        Some(trigger),
                    )
                } else {
                    (Duration::ZERO, self.overview_mode.active_trigger().cloned())
                };
            self.overview_mode = OverviewMode::Ended(trigger, Instant::now() - reverse_duration);
        }
    }

    pub fn overview_mode(&self) -> (OverviewMode, Option<SwapIndicator>) {
        if let OverviewMode::Started(trigger, timestamp) = &self.overview_mode
            && Instant::now().duration_since(*timestamp) > ANIMATION_DURATION
        {
            return (
                OverviewMode::Active(trigger.clone()),
                self.swap_indicator.clone(),
            );
        }
        if let OverviewMode::Ended(_, timestamp) = &self.overview_mode
            && Instant::now().duration_since(*timestamp) > ANIMATION_DURATION
        {
            return (OverviewMode::None, None);
        }

        (self.overview_mode.clone(), self.swap_indicator.clone())
    }

    pub fn set_resize_mode(
        &mut self,
        enabled: Option<(shortcuts::Binding, ResizeDirection)>,
        config: &Config,
        evlh: LoopHandle<'static, crate::state::State>,
    ) {
        if let Some((pattern, direction)) = enabled {
            if let ResizeMode::Started(old_pattern, _, old_direction) = &mut self.resize_mode {
                *old_pattern = pattern;
                *old_direction = direction;
            } else {
                self.resize_mode = ResizeMode::Started(pattern, Instant::now(), direction);
            }
            {
                use cosmic_settings_config::shortcuts::action::Action;
                let dir_val: u32 = match direction {
                    ResizeDirection::Outwards => 1,
                    ResizeDirection::Inwards => 2,
                };
                let s1 = config
                    .shortcuts
                    .iter()
                    .find_map(|(pattern, action)| {
                        (*action == Action::Resizing(ResizeDirection::Outwards))
                            .then_some(format!("{:?}: ", pattern))
                    })
                    .unwrap_or_default();
                let s2 = config
                    .shortcuts
                    .iter()
                    .find_map(|(pattern, action)| {
                        (*action == Action::Resizing(ResizeDirection::Inwards))
                            .then_some(format!("{:?}: ", pattern))
                    })
                    .unwrap_or_default();
                evlh.clone().insert_idle(move |state| {
                    state.common.shell_overlay_state.send_indicator_show(
                        3, 0x0F, dir_val, s1, s2,
                    );
                });
            }
            self.resize_indicator = Some(resize_indicator(
                direction,
                config,
                evlh,
            ));
        } else if let Some(direction) = self.resize_mode.active_direction() {
            self.resize_mode = ResizeMode::Ended(Instant::now(), direction);
            if let Some((_, direction, edge, _, _, _)) = self.resize_state.as_ref() {
                self.finish_resize(*direction, *edge);
            }
        }
    }

    pub fn resize_mode(&self) -> (ResizeMode, Option<ResizeIndicator>) {
        if let ResizeMode::Started(binding, timestamp, direction) = &self.resize_mode
            && Instant::now().duration_since(*timestamp) > ANIMATION_DURATION
        {
            return (
                ResizeMode::Active(binding.clone(), *direction),
                self.resize_indicator.clone(),
            );
        }
        if let ResizeMode::Ended(timestamp, _) = self.resize_mode
            && Instant::now().duration_since(timestamp) > ANIMATION_DURATION
        {
            return (ResizeMode::None, None);
        }

        (self.resize_mode.clone(), self.resize_indicator.clone())
    }

    pub fn stacking_indicator(
        &self,
        output: &Output,
        layer: ManagedLayer,
    ) -> Option<Rectangle<i32, Local>> {
        match layer {
            ManagedLayer::Sticky => self
                .workspaces
                .sets
                .get(output)
                .and_then(|set| set.sticky_layer.stacking_indicator()),
            ManagedLayer::Floating => self
                .active_space(output)?
                .floating_layer
                .stacking_indicator(),
            ManagedLayer::Tiling => self.active_space(output)?.tiling_layer.stacking_indicator(),
            ManagedLayer::Fullscreen => None,
        }
    }

    pub fn appearance_config(&self) -> AppearanceConfig {
        self.appearance_conf
    }

    pub fn trigger_zoom(
        &mut self,
        seat: &Seat<State>,
        output: Option<&Output>,
        level: f64,
        zoom_config: &ZoomConfig,
        animate: bool,
        loop_handle: &LoopHandle<'static, State>,
    ) {
        if self.zoom_state.is_none() && level == 1. {
            return;
        }

        let outputs = output.map(|o| vec![o]).unwrap_or(self.outputs().collect());
        if self.zoom_state.is_none() {
            for output in self.outputs() {
                output.user_data().insert_if_missing_threadsafe(|| {
                    Mutex::new(OutputZoomState::new(
                        seat,
                        output,
                        1.0,
                        zoom_config.increment,
                        zoom_config.view_moves,
                    ))
                });
            }
        }

        let mut toggled = self.zoom_state.is_none();
        if let Some(old_state) = self.zoom_state.as_ref()
            && &old_state.seat != seat
        {
            return;
        }

        for output in &outputs {
            let output_state = output.user_data().get::<Mutex<OutputZoomState>>().unwrap();
            output_state.lock().unwrap().update(
                level,
                animate,
                zoom_config.view_moves,
                zoom_config.increment,
            );
        }

        let all_outputs_off = self.outputs().all(|o| {
            o.user_data()
                .get::<Mutex<OutputZoomState>>()
                .unwrap()
                .lock()
                .unwrap()
                .current_level()
                == 1.0
        });
        toggled = toggled || all_outputs_off;

        if toggled {
            let value = !all_outputs_off;
            let _ = loop_handle.insert_idle(move |state| {
                state.common.a11y_state.set_screen_magnifier(value);
            });
        }

        let movement_u32 = match zoom_config.view_moves {
            ZoomMovement::Continuously => 1,
            ZoomMovement::OnEdge => 2,
            ZoomMovement::Centered => 3,
        };
        self.pending_zoom_event = Some(ZoomProtocolEvent::Show {
            level,
            increment: zoom_config.increment,
            movement: movement_u32,
        });

        self.zoom_state = Some(ZoomState {
            seat: seat.clone(),
            show_overlay: zoom_config.show_overlay,
            increment: zoom_config.increment,
            movement: zoom_config.view_moves,
        });
    }

    pub fn update_focal_point(
        &mut self,
        seat: &Seat<State>,
        original_position: Point<f64, Global>,
        movement: ZoomMovement,
    ) {
        if let Some(state) = self.zoom_state.as_mut() {
            if &state.seat != seat {
                return;
            }

            let cursor_position = seat.get_pointer().unwrap().current_location().as_global();

            state.update_focal_point(
                &seat.active_output(),
                cursor_position,
                original_position,
                movement,
            );
        }
    }

    pub fn zoom_state(&self) -> Option<&ZoomState> {
        self.zoom_state.as_ref()
    }

    fn refresh(
        &mut self,
        xdg_activation_state: &XdgActivationState,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        match &self.overview_mode {
            OverviewMode::Started(trigger, timestamp)
                if Instant::now().duration_since(*timestamp) > ANIMATION_DURATION =>
            {
                self.overview_mode = OverviewMode::Active(trigger.clone());
            }
            OverviewMode::Ended(_, timestamp)
                if Instant::now().duration_since(*timestamp) > ANIMATION_DURATION =>
            {
                self.overview_mode = OverviewMode::None;
                if self.swap_indicator.is_some() {
                    self.pending_indicator_hides.push(2);
                }
                self.swap_indicator = None;
            }
            _ => {}
        }

        match &self.resize_mode {
            ResizeMode::Started(binding, timestamp, direction)
                if Instant::now().duration_since(*timestamp) > ANIMATION_DURATION =>
            {
                self.resize_mode = ResizeMode::Active(binding.clone(), *direction);
            }
            ResizeMode::Ended(timestamp, _)
                if Instant::now().duration_since(*timestamp) > ANIMATION_DURATION =>
            {
                self.resize_mode = ResizeMode::None;
                if self.resize_indicator.is_some() {
                    self.pending_indicator_hides.push(3);
                }
                self.resize_indicator = None;
            }
            _ => {}
        }

        if self.zoom_state.is_some() {
            let mut all_outputs_off = true;
            for output in self.outputs() {
                all_outputs_off = all_outputs_off
                    && output
                        .user_data()
                        .get::<Mutex<OutputZoomState>>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .refresh();
            }

            if all_outputs_off {
                self.zoom_state.take();
                self.pending_zoom_event = Some(ZoomProtocolEvent::Hide);
            }
        }

        self.workspaces
            .refresh(workspace_state, xdg_activation_state);

        for output in self.outputs() {
            let mut map = layer_map_for_output(output);
            map.cleanup();
        }

        self.override_redirect_windows.retain(|or| or.alive());
        self.override_redirect_windows
            .iter()
            .for_each(|or| or.refresh());

        self.pending_layers
            .retain(|pending| pending.surface.alive());
        self.pending_windows
            .retain(|pending| pending.surface.alive());
    }

    pub fn update_pointer_position(&mut self, location: Point<f64, Local>, output: &Output) {
        for (o, set) in self.workspaces.sets.iter_mut() {
            if o == output {
                set.sticky_layer.update_pointer_position(Some(location));
                for (i, workspace) in set.workspaces.iter_mut().enumerate() {
                    if i == set.active {
                        workspace
                            .update_pointer_position(Some(location), self.overview_mode.clone());
                    } else {
                        workspace.update_pointer_position(None, self.overview_mode.clone());
                    }
                }
            } else {
                set.sticky_layer.update_pointer_position(None);
                for workspace in &mut set.workspaces {
                    workspace.update_pointer_position(None, self.overview_mode.clone());
                }
            }
        }
    }

    pub fn remap_unfullscreened_window(
        &mut self,
        surface: CosmicSurface,
        state: Option<FullscreenRestoreState>,
        loop_handle: &LoopHandle<'static, State>,
    ) -> CosmicMapped {
        let window = CosmicMapped::from(CosmicWindow::new(
            surface,
            loop_handle.clone(),
            self.appearance_conf,
        ));

        if let Some(FullscreenRestoreState::Sticky { output, state, .. }) = &state {
            let output = output
                .upgrade()
                .unwrap_or_else(|| self.seats.last_active().active_output());
            toplevel_enter_output(&window.active_window(), &output);
            let set = self
                .workspaces
                .sets
                .get_mut(&output)
                .or(self.workspaces.backup_set.as_mut())
                .unwrap();
            set.sticky_layer.map_internal(
                window.clone(),
                Some(state.geometry.loc),
                Some(state.geometry.size.as_logical()),
                Some(set.output.geometry().to_local(&set.output)),
            );
            return window;
        }

        let seat = self.seats.last_active();
        let workspace = match &state {
            Some(FullscreenRestoreState::Floating { workspace, .. })
            | Some(FullscreenRestoreState::Tiling { workspace, .. }) => {
                let workspace = self.workspaces.space_for_handle_mut(workspace);
                let workspace = match workspace {
                    Some(workspace) => workspace,
                    None => self.workspaces.active_mut(&seat.active_output()).unwrap(),
                };
                toplevel_enter_output(&window.active_window(), &workspace.output);
                toplevel_enter_workspace(&window.active_window(), &workspace.handle);

                workspace
            }
            None => self.workspaces.active_mut(&seat.active_output()).unwrap(),
            Some(FullscreenRestoreState::Sticky { .. }) => unreachable!(),
        };
        let fullscreen_geometry = workspace.output.geometry().to_local(&workspace.output);

        match state {
            None => {
                toplevel_enter_output(&window.active_window(), &workspace.output);
                toplevel_enter_workspace(&window.active_window(), &workspace.handle);

                if workspace.tiling_enabled {
                    workspace.tiling_layer.remap(
                        window.clone(),
                        Some(fullscreen_geometry),
                        None,
                        Some(workspace.focus_stack.get(seat).iter()),
                    );
                } else {
                    workspace.floating_layer.map_internal(
                        window.clone(),
                        None,
                        None,
                        Some(fullscreen_geometry),
                    );
                }
            }
            Some(FullscreenRestoreState::Floating {
                state:
                    FloatingRestoreData {
                        was_maximized,
                        was_snapped,
                        geometry,
                        ..
                    },
                ..
            }) => {
                workspace.floating_layer.map_internal(
                    window.clone(),
                    Some(geometry.loc),
                    Some(geometry.size.as_logical()),
                    Some(fullscreen_geometry),
                );
                if was_maximized {
                    let mut state = window.maximized_state.lock().unwrap();
                    *state = Some(MaximizedState {
                        original_geometry: geometry,
                        original_layer: ManagedLayer::Floating,
                    });
                    std::mem::drop(state);
                    workspace.floating_layer.map_maximized(
                        window.clone(),
                        fullscreen_geometry,
                        true,
                    );
                } else if let Some(corners) = was_snapped {
                    workspace.floating_layer.snap_to_corner(&window, &corners);
                }
            }
            Some(FullscreenRestoreState::Tiling {
                state:
                    TilingRestoreData {
                        state,
                        was_maximized,
                    },
                ..
            }) => {
                if workspace.tiling_enabled {
                    let focus_stack = workspace.focus_stack.get(seat);
                    workspace.tiling_layer.remap(
                        window.clone(),
                        Some(fullscreen_geometry),
                        state,
                        Some(focus_stack.iter()),
                    );
                    if was_maximized {
                        let previous_geometry =
                            workspace.tiling_layer.element_geometry(&window).unwrap();
                        let mut state = window.maximized_state.lock().unwrap();
                        *state = Some(MaximizedState {
                            original_geometry: previous_geometry,
                            original_layer: ManagedLayer::Tiling,
                        });
                        std::mem::drop(state);
                        workspace.floating_layer.map_maximized(
                            window.clone(),
                            fullscreen_geometry,
                            true,
                        );
                    }
                } else {
                    workspace.floating_layer.map_internal(
                        window.clone(),
                        None,
                        None,
                        Some(fullscreen_geometry),
                    );

                    if was_maximized {
                        let geometry = workspace.floating_layer.element_geometry(&window).unwrap();
                        let mut state = window.maximized_state.lock().unwrap();
                        *state = Some(MaximizedState {
                            original_geometry: geometry,
                            original_layer: ManagedLayer::Floating,
                        });
                        std::mem::drop(state);
                        workspace.floating_layer.map_maximized(
                            window.clone(),
                            fullscreen_geometry,
                            true,
                        );
                    }
                }
            }
            Some(FullscreenRestoreState::Sticky { .. }) => unreachable!(),
        }

        window
    }

    #[must_use]
    pub fn map_window(
        &mut self,
        window: &CosmicSurface,
        toplevel_info: &mut ToplevelInfoState<State, CosmicSurface>,
        workspace_state: &mut WorkspaceState<State>,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget> {
        let pos = self
            .pending_windows
            .iter()
            .position(|pending| &pending.surface == window)
            .unwrap();
        let PendingWindow {
            surface: window,
            seat,
            fullscreen: output,
            maximized: should_be_maximized,
        } = self.pending_windows.remove(pos);

        let parent_is_sticky = if let Some(toplevel) = window.0.toplevel() {
            if let Some(parent) = toplevel.parent() {
                if let Some(elem) = self.element_for_surface(&parent) {
                    self.workspaces
                        .sets
                        .values()
                        .any(|set| set.sticky_layer.mapped().any(|m| m == elem))
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        let pending_activation = self.pending_activations.remove(&(&window).into());
        let workspace_handle = match pending_activation {
            Some(ActivationContext::Workspace(handle)) => Some(handle),
            _ => None,
        };

        let should_be_fullscreen = output.is_some();
        let mut output = output.unwrap_or_else(|| seat.active_output());

        // this is beyond stupid, just to make the borrow checker happy
        let workspace = if let Some(handle) = workspace_handle.filter(|handle| {
            self.workspaces
                .spaces()
                .any(|space| &space.handle == handle)
        }) {
            self.workspaces
                .spaces_mut()
                .find(|space| space.handle == handle)
                .unwrap()
        } else {
            self.workspaces.active_mut(&output).unwrap() // a seat's active output always has a workspace
        };
        if output != workspace.output {
            output = workspace.output.clone();
        }

        let active_handle = self.active_space(&output).unwrap().handle;
        let workspace = if let Some(handle) = workspace_handle.filter(|handle| {
            self.workspaces
                .spaces()
                .any(|space| &space.handle == handle)
        }) {
            self.workspaces
                .spaces_mut()
                .find(|space| space.handle == handle)
                .unwrap()
        } else {
            self.workspaces.active_mut(&output).unwrap()
        };

        toplevel_info.new_toplevel(&window, workspace_state);
        toplevel_enter_output(&window, &output);
        toplevel_enter_workspace(&window, &workspace.handle);

        let mut workspace_state = workspace_state.update();

        let workspace_output = workspace.output.clone();
        let was_activated = workspace_handle.is_some()
            && (workspace_output != seat.active_output() || active_handle != workspace.handle);
        let workspace_handle = workspace.handle;
        let is_dialog = layout::is_dialog(&window);
        let floating_exception = layout::has_floating_exception(&self.tiling_exceptions, &window);
        // TOML window rules override built-in detection.
        let rule_override = layout::check_window_rules(&self.window_rules, &window);

        if should_be_fullscreen {
            if let Some((surface, state, _)) = workspace.map_fullscreen(&window, &seat, None, None)
            {
                toplevel_leave_output(&surface, &workspace.output);
                toplevel_leave_workspace(&surface, &workspace.handle);
                self.remap_unfullscreened_window(surface, state, loop_handle);
            }
            if was_activated {
                workspace_state.add_workspace_state(&workspace_handle, WState::Urgent);
            }

            return (workspace_output == seat.active_output() && active_handle == workspace_handle)
                .then_some(KeyboardFocusTarget::Fullscreen(window));
        }

        let maybe_focused = workspace.focus_stack.get(&seat).iter().next().cloned();
        if let Some(FocusTarget::Window(focused)) = maybe_focused
            && (focused.is_stack() && !is_dialog && !should_be_maximized)
            && !(workspace.is_tiled(&focused.active_window()) && floating_exception)
        {
            focused.stack_ref().unwrap().add_window(window, None, None);
            if was_activated {
                workspace_state.add_workspace_state(&workspace_handle, WState::Urgent);
            }
            return (workspace_output == seat.active_output() && active_handle == workspace_handle)
                .then_some(KeyboardFocusTarget::Element(focused));
        }

        let mapped = CosmicMapped::from(CosmicWindow::new(
            window.clone(),
            loop_handle.clone(),
            self.appearance_conf,
        ));
        #[cfg(feature = "debug")]
        {
            mapped.set_debug(self.debug_active);
        }

        let workspace_empty = workspace.mapped().next().is_none();
        // TOML rules have highest priority, then dialog/exception checks.
        let should_float = match rule_override {
            Some(force_float) => force_float,
            None => is_dialog || floating_exception,
        };
        if should_float || !workspace.tiling_enabled {
            workspace.floating_layer.map(mapped.clone(), None);
            // In monocle mode, maximize new non-dialog windows.
            if workspace.is_monocle() && !should_float {
                let output_geo = workspace.output.geometry();
                mapped.set_maximized(true);
                mapped.set_geometry(output_geo);
            }
        } else {
            for mapped in workspace
                .mapped()
                .filter(|m| m.maximized_state.lock().unwrap().is_some())
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
            {
                workspace.unmaximize_request(&mapped);
            }
            let focus_stack = workspace.focus_stack.get(&seat);
            workspace
                .tiling_layer
                .map(mapped.clone(), Some(focus_stack.iter()), None);
        }

        if parent_is_sticky {
            self.toggle_sticky(&seat, &mapped);
        }

        if should_be_maximized {
            self.maximize_request(&mapped, &seat, false, loop_handle);
        }

        let new_target = if (workspace_output == seat.active_output()
            && active_handle == workspace_handle)
            || parent_is_sticky
        {
            // TODO: enforce focus stealing prevention by also checking the same rules as for the else case.
            Some(KeyboardFocusTarget::from(mapped.clone()))
        } else {
            if workspace_empty || was_activated {
                self.append_focus_stack(mapped, &seat);
                workspace_state.add_workspace_state(&workspace_handle, WState::Urgent);
            }
            None
        };

        let active_space = self.active_space(&output).unwrap();
        for mapped in active_space.mapped() {
            self.update_reactive_popups(mapped);
        }

        // DECO-DEBUG: log map-time SSD decision for the just-mapped
        // window. `is_decorated(pending=false)` reads current
        // committed state. If Wayland-client with kde or xdg
        // decoration set to Server/ServerSide, returns false →
        // has_ssd = true. X11 surfaces go through their own path.
        tracing::info!(
            "DECO-DEBUG map_window app_id={:?} title={:?} is_decorated(committed)={} is_decorated(pending)={}",
            window.app_id(), window.title(),
            window.is_decorated(false), window.is_decorated(true)
        );

        new_target
    }

    pub fn map_override_redirect(&mut self, window: X11Surface) {
        let geo = window.geometry();
        for (output, overlap) in self.outputs().cloned().filter_map(|o| {
            o.geometry()
                .as_logical()
                .intersection(geo)
                .map(|overlap| (o, overlap))
        }) {
            window.output_enter(&output, overlap);
        }

        self.override_redirect_windows.push(window);
    }

    #[must_use]
    pub fn map_layer(&mut self, layer_surface: &LayerSurface) -> Option<KeyboardFocusTarget> {
        let pos = self
            .pending_layers
            .iter()
            .position(|pending| &pending.surface == layer_surface)
            .unwrap();
        let pending = self.pending_layers.remove(pos);

        let wants_focus = {
            with_states(pending.surface.wl_surface(), |states| {
                let mut state = states.cached_state.get::<LayerSurfaceCachedState>();
                matches!(state.current().layer, Layer::Top | Layer::Overlay)
                    && state.current().keyboard_interactivity != KeyboardInteractivity::None
            })
        };

        {
            let mut map = layer_map_for_output(&pending.output);
            map.map_layer(&pending.surface).unwrap();
        }
        for workspace in self.workspaces.spaces_mut() {
            workspace.tiling_layer.recalculate();
        }

        wants_focus.then(|| pending.surface.into())
    }

    pub fn unmap_surface<S>(
        &mut self,
        surface: &S,
        seat: &Seat<State>,
        toplevel_info: &mut ToplevelInfoState<State, CosmicSurface>,
    ) -> Option<PendingWindow>
    where
        CosmicSurface: PartialEq<S>,
    {
        for set in self.workspaces.sets.values_mut() {
            let sticky_res = set.sticky_layer.mapped().find_map(|m| {
                m.windows()
                    .position(|(s, _)| &s == surface)
                    .map(|idx| (idx, m.clone()))
            });
            let surface = if let Some((idx, mapped)) = sticky_res {
                if mapped.is_stack() {
                    mapped.stack_ref().unwrap().remove_idx(idx)
                } else {
                    set.sticky_layer.unmap(&mapped, None);
                    Some(mapped.active_window())
                }
            } else if let Some(idx) = set
                .minimized_windows
                .iter()
                .position(|w| w.windows().any(|s| &s == surface))
            {
                if set
                    .minimized_windows
                    .get(idx)
                    .unwrap()
                    .mapped()
                    .is_some_and(CosmicMapped::is_stack)
                {
                    let window = set
                        .minimized_windows
                        .get_mut(idx)
                        .unwrap()
                        .mapped_mut()
                        .unwrap();
                    let stack = window.stack_ref().unwrap();
                    let idx = stack.surfaces().position(|s| &s == surface);
                    idx.and_then(|idx| stack.remove_idx(idx))
                } else {
                    Some(
                        set.minimized_windows
                            .remove(idx)
                            .mapped()
                            .unwrap()
                            .active_window(),
                    )
                }
            } else if let Some((surface, _)) = set
                .workspaces
                .iter_mut()
                .find_map(|w| w.unmap_surface(surface))
            {
                Some(surface)
            } else {
                None
            };

            if let Some(surface) = surface {
                toplevel_info.remove_toplevel(&surface);
                return Some(PendingWindow {
                    surface,
                    seat: seat.clone(),
                    fullscreen: None,
                    maximized: false,
                });
            }
        }

        None
    }

    pub fn move_current(
        &mut self,
        seat: &Seat<State>,
        to: (&Output, Option<usize>),
        follow: bool,
        direction: Option<Direction>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        evlh: &LoopHandle<'static, State>,
    ) -> Result<Option<(KeyboardFocusTarget, Point<i32, Global>)>, InvalidWorkspaceIndex> {
        let (to_output, to_idx) = to;
        let to_idx = to_idx.unwrap_or(self.workspaces.active_num(to_output).1);
        let from_output = seat.focused_or_active_output();
        let from_idx = self.workspaces.active_num(&from_output).1;

        if &from_output == to_output && to_idx == self.workspaces.active_num(&from_output).1 {
            return Ok(None);
        }

        if &from_output == to_output
            && to_idx.checked_sub(1).is_some_and(|idx| idx == from_idx)
            && to_idx == self.workspaces.len(to_output) - 1
            && self
                .workspaces
                .get(from_idx, &from_output)
                .is_some_and(|w| w.len() == 1)
            && self
                .workspaces
                .get(to_idx, to_output)
                .is_some_and(|w| w.is_empty())
        {
            return Err(InvalidWorkspaceIndex);
        }

        let to = self
            .workspaces
            .get(to_idx, to_output)
            .map(|ws| ws.handle)
            .ok_or(InvalidWorkspaceIndex)?;

        let from_workspace = self
            .workspaces
            .active_mut(&from_output)
            .ok_or(InvalidWorkspaceIndex)?;
        let from = from_workspace.handle;

        match seat.get_keyboard().unwrap().current_focus() {
            Some(KeyboardFocusTarget::Group(WindowGroup {
                node, focus_stack, ..
            })) => {
                let new_pos = if follow {
                    seat.set_active_output(to_output);
                    self.workspaces
                        .idx_for_handle(to_output, &to)
                        .and_then(|to_idx| {
                            self.activate(
                                to_output,
                                to_idx,
                                WorkspaceDelta::new_shortcut(),
                                workspace_state,
                            )
                            .ok()
                        })
                } else {
                    None
                };

                let spaces = self.workspaces.spaces_mut();
                let (mut from_w, mut other_w) = spaces.partition::<Vec<_>, _>(|w| w.handle == from);
                if let Some(from_workspace) = from_w.get_mut(0)
                    && let Some(to_workspace) = other_w.iter_mut().find(|w| w.handle == to)
                {
                    {
                        let mut stack = to_workspace.focus_stack.get_mut(seat);
                        for elem in focus_stack.iter().flat_map(|node_id| {
                            from_workspace.tiling_layer.element_for_node(node_id)
                        }) {
                            stack.append(elem.clone());
                        }
                    }

                    if to_workspace.tiling_enabled {
                        for mapped in to_workspace
                            .mapped()
                            .filter(|m| m.maximized_state.lock().unwrap().is_some())
                            .cloned()
                            .collect::<Vec<_>>()
                            .into_iter()
                        {
                            to_workspace.unmaximize_request(&mapped);
                        }
                    }

                    let res = TilingLayout::move_tree(
                        &mut from_workspace.tiling_layer,
                        &mut to_workspace.tiling_layer,
                        &to,
                        seat,
                        to_workspace.focus_stack.get(seat).iter(),
                        NodeDesc {
                            handle: from,
                            node,
                            stack_window: None,
                            focus_stack,
                        },
                        direction,
                    );
                    from_workspace.refresh_focus_stack();
                    to_workspace.refresh_focus_stack();

                    if !to_workspace.tiling_enabled {
                        to_workspace.tiling_enabled = true;
                        for mapped in to_workspace
                            .tiling_layer
                            .mapped()
                            .map(|(mapped, _)| mapped.clone())
                            .collect::<Vec<_>>()
                            .into_iter()
                        {
                            to_workspace.toggle_floating_window(seat, &mapped);
                        }
                        to_workspace.tiling_enabled = false;
                    }

                    return Ok(res.zip(new_pos));
                }

                Ok(None)
            }
            Some(KeyboardFocusTarget::Fullscreen(surface)) => Ok(self.move_window(
                Some(seat),
                &surface,
                &from,
                &to,
                follow,
                direction,
                workspace_state,
                evlh,
            )),
            Some(KeyboardFocusTarget::Element(mapped)) => Ok(self.move_element(
                Some(seat),
                &mapped,
                &from,
                &to,
                follow,
                direction,
                workspace_state,
            )),
            _ => Ok(None),
        }
    }

    #[must_use]
    pub fn move_window(
        &mut self,
        seat: Option<&Seat<State>>,
        window: &CosmicSurface,
        from: &WorkspaceHandle,
        to: &WorkspaceHandle,
        follow: bool,
        direction: Option<Direction>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
        evlh: &LoopHandle<'static, State>,
    ) -> Option<(KeyboardFocusTarget, Point<i32, Global>)> {
        let from_output = self.workspaces.space_for_handle(from)?.output.clone();
        let to_output = self.workspaces.space_for_handle(to)?.output.clone();
        let to_is_tiling = self.workspaces.space_for_handle(to).unwrap().tiling_enabled;

        let from_workspace = self.workspaces.space_for_handle_mut(from).unwrap(); // checked above

        let is_minimized = window.is_minimized();
        let mut window_state = from_workspace.unmap_surface(window)?.1;

        toplevel_leave_workspace(window, from);
        if from_output != to_output {
            toplevel_leave_output(window, &from_output);
            toplevel_enter_output(window, &to_output);
        }
        toplevel_enter_workspace(window, to);

        // we can't restore to a given position
        if let WorkspaceRestoreData::Tiling(Some(state)) = &mut window_state {
            state.state.take();
        }
        // update fullscreen state to restore to the new workspace
        if let WorkspaceRestoreData::Fullscreen(Some(FullscreenRestoreData {
            previous_state: previous,
            ..
        })) = &mut window_state
        {
            if to_is_tiling && !from_workspace.tiling_enabled {
                *previous = FullscreenRestoreState::Tiling {
                    workspace: *to,
                    state: TilingRestoreData {
                        state: None,
                        was_maximized: previous.was_maximized(),
                    },
                };
            } else if let FullscreenRestoreState::Tiling { workspace, .. }
            | FullscreenRestoreState::Floating { workspace, .. } = previous
            {
                *workspace = *to;
            }
        }

        if is_minimized {
            let to_workspace = self.workspaces.space_for_handle_mut(to).unwrap(); // checked above
            let minimized_window = match window_state {
                WorkspaceRestoreData::Floating(Some(previous)) => {
                    let window = CosmicMapped::from(CosmicWindow::new(
                        window.clone(),
                        evlh.clone(),
                        self.appearance_conf,
                    ));
                    window.set_minimized(true);
                    MinimizedWindow::Floating { window, previous }
                }
                WorkspaceRestoreData::Tiling(Some(previous)) => {
                    let window = CosmicMapped::from(CosmicWindow::new(
                        window.clone(),
                        evlh.clone(),
                        self.appearance_conf,
                    ));
                    window.set_minimized(true);
                    MinimizedWindow::Tiling { window, previous }
                }
                WorkspaceRestoreData::Fullscreen(previous) => {
                    window.set_minimized(true);
                    MinimizedWindow::Fullscreen {
                        surface: window.clone(),
                        previous,
                    }
                }
                _ => {
                    unreachable!()
                } // MinimizedWindow always has restore data
            };
            to_workspace.minimized_windows.push(minimized_window);
            return None;
        }

        for mapped in from_workspace
            .mapped()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
        {
            self.update_reactive_popups(&mapped);
        }

        let new_pos = if follow {
            if let Some(seat) = seat {
                seat.set_active_output(&to_output);
            }
            self.workspaces
                .idx_for_handle(&to_output, to)
                .and_then(|to_idx| {
                    self.activate(
                        &to_output,
                        to_idx,
                        WorkspaceDelta::new_shortcut(),
                        workspace_state,
                    )
                    .ok()
                })
        } else {
            None
        };

        let to_workspace = self.workspaces.space_for_handle_mut(to).unwrap(); // checked above
        let to_mapped = to_workspace.mapped().cloned().collect::<Vec<_>>();

        let focus_target: KeyboardFocusTarget =
            if !matches!(window_state, WorkspaceRestoreData::Fullscreen(_))
                && !to_workspace.tiling_enabled
            {
                let mapped = CosmicMapped::from(CosmicWindow::new(
                    window.clone(),
                    evlh.clone(),
                    self.appearance_conf,
                ));
                let position = match window_state {
                    WorkspaceRestoreData::Floating(Some(data)) => Some(
                        data.position_relative(to_workspace.output.geometry().size.as_logical()),
                    ),
                    _ => None,
                };
                to_workspace.floating_layer.map(mapped.clone(), position);
                mapped.into()
            } else if !matches!(window_state, WorkspaceRestoreData::Fullscreen(_))
                && to_workspace.tiling_enabled
            {
                let mapped = CosmicMapped::from(CosmicWindow::new(
                    window.clone(),
                    evlh.clone(),
                    self.appearance_conf,
                ));
                for mapped in to_workspace
                    .mapped()
                    .filter(|m| m.maximized_state.lock().unwrap().is_some())
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                {
                    to_workspace.unmaximize_request(&mapped);
                }
                let focus_stack = seat.map(|seat| to_workspace.focus_stack.get(seat));
                to_workspace.tiling_layer.map(
                    mapped.clone(),
                    focus_stack.as_ref().map(|x| x.iter()),
                    direction,
                );
                mapped.into()
            } else if let WorkspaceRestoreData::Fullscreen(previous) = window_state {
                if let Some((old_surface, previous_state, _)) = to_workspace.map_fullscreen(
                    window,
                    None,
                    previous.clone().map(|p| p.previous_state),
                    previous.map(|p| p.previous_geometry),
                ) {
                    self.remap_unfullscreened_window(old_surface, previous_state, evlh);
                }
                window.clone().into()
            } else {
                unreachable!() // TODO: sticky
            };

        for mapped in to_mapped.into_iter() {
            self.update_reactive_popups(&mapped);
        }

        new_pos.map(|pos| (focus_target, pos))
    }

    #[must_use]
    pub fn move_element(
        &mut self,
        seat: Option<&Seat<State>>,
        mapped: &CosmicMapped,
        from: &WorkspaceHandle,
        to: &WorkspaceHandle,
        follow: bool,
        direction: Option<Direction>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) -> Option<(KeyboardFocusTarget, Point<i32, Global>)> {
        let from_output = self.workspaces.space_for_handle(from)?.output.clone();
        let to_output = self.workspaces.space_for_handle(to)?.output.clone();
        // Snapshot the source/target tiling modes and the window's current
        // geometry before we touch anything. Useful for diagnosing a
        // reported "window resizes unexpectedly after move" symptom: the
        // most common cause is a tiling-mode mismatch between source and
        // target workspace (floating on ws A, tiling on ws B) — moving
        // the window forces it into the other layout and the resize is
        // logically correct, but surprising UX. The logs make that
        // explicit so we can tell apart "resize because of tiling" from
        // "resize because of an actual bug".
        let from_ws = self.workspaces.space_for_handle(from)?;
        let to_ws = self.workspaces.space_for_handle(to)?;
        let from_tiled = from_ws.tiling_enabled;
        let to_tiled = to_ws.tiling_enabled;
        let pre_geo = from_ws.element_geometry(mapped);
        let pre_size = mapped.geometry().size;
        tracing::info!(
            "Shell::move_element: from={:?} to={:?} follow={} \
             from_tiled={} to_tiled={} pre_element_geo={:?} pre_mapped_size={:?}",
            from,
            to,
            follow,
            from_tiled,
            to_tiled,
            pre_geo,
            pre_size,
        );

        let from_workspace = self.workspaces.space_for_handle_mut(from).unwrap(); // checked above
        let window_state = from_workspace.unmap_element(mapped)?;
        let elements = from_workspace.mapped().cloned().collect::<Vec<_>>();

        for (toplevel, _) in mapped.windows() {
            toplevel_leave_workspace(&toplevel, from);
            if from_output != to_output {
                toplevel_leave_output(&toplevel, &from_output);
            }
        }
        for mapped in elements.into_iter() {
            self.update_reactive_popups(&mapped);
        }
        let new_pos = if follow {
            if let Some(seat) = seat {
                seat.set_active_output(&to_output);
            }
            self.workspaces
                .idx_for_handle(&to_output, to)
                .and_then(|to_idx| {
                    self.activate(
                        &to_output,
                        to_idx,
                        WorkspaceDelta::new_shortcut(),
                        workspace_state,
                    )
                    .ok()
                })
        } else {
            None
        };

        let to_workspace = self.workspaces.space_for_handle_mut(to).unwrap(); // checked above
        if !to_workspace.tiling_enabled {
            let (position, was_maximized, was_snapped) = match &window_state {
                WorkspaceRestoreData::Floating(Some(data)) => (
                    Some(data.position_relative(to_workspace.output.geometry().size.as_logical())),
                    data.was_maximized,
                    data.was_snapped,
                ),
                _ => (None, false, None),
            };
            to_workspace.floating_layer.map(mapped.clone(), position);
            if was_maximized {
                let geometry = to_workspace
                    .floating_layer
                    .element_geometry(mapped)
                    .unwrap();
                *mapped.maximized_state.lock().unwrap() = Some(MaximizedState {
                    original_geometry: geometry,
                    original_layer: ManagedLayer::Floating,
                });
                to_workspace
                    .floating_layer
                    .map_maximized(mapped.clone(), geometry, false);
            } else if let Some(corners) = was_snapped {
                to_workspace.floating_layer.snap_to_corner(mapped, &corners);
            }
        } else {
            for mapped in to_workspace
                .mapped()
                .filter(|m| m.maximized_state.lock().unwrap().is_some())
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
            {
                to_workspace.unmaximize_request(&mapped);
            }
            let focus_stack = seat.map(|seat| to_workspace.focus_stack.get(seat));
            to_workspace.tiling_layer.map(
                mapped.clone(),
                focus_stack.as_ref().map(|x| x.iter()),
                direction,
            );
        }

        let focus_target = KeyboardFocusTarget::from(mapped.clone());

        for mapped in to_workspace
            .mapped()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
        {
            self.update_reactive_popups(&mapped);
        }
        for (toplevel, _) in mapped.windows() {
            if from_output != to_output {
                toplevel_enter_output(&toplevel, &to_output);
            }
            toplevel_enter_workspace(&toplevel, to);
        }

        // After-state snapshot: geometry on the target workspace + the
        // window's own reported size. Compare with the pre-move snapshot
        // above to tell whether the move caused a layout-driven resize.
        let post_geo = self.workspaces.space_for_handle(to)
            .and_then(|ws| ws.element_geometry(mapped));
        let post_size = mapped.geometry().size;
        tracing::info!(
            "Shell::move_element: done; post_element_geo={:?} post_mapped_size={:?}",
            post_geo,
            post_size,
        );

        new_pos.map(|pos| (focus_target, pos))
    }

    pub fn update_reactive_popups(&self, mapped: &CosmicMapped) {
        if let Some(workspace) = self.space_for(mapped)
            && let Some(element_loc) = workspace
                .element_geometry(mapped)
                .map(|geo| geo.loc.to_global(&workspace.output))
        {
            for (window, offset) in mapped.windows() {
                if let Some(toplevel) = window.0.toplevel() {
                    let window_geo_offset = window.geometry().loc.as_global();
                    update_reactive_popups(
                        toplevel,
                        element_loc + offset.as_global() + window_geo_offset,
                        self.outputs(),
                    );
                }
            }
        }
    }

    /// Handle a context menu request from a client or input grab.
    ///
    /// When a desktop-shell client is connected, the menu is sent via the
    /// `lunaris-shell-overlay-v1` protocol and rendered by desktop-shell.
    /// When no shell client is connected, the grab falls back to Iced rendering.
    pub fn menu_request(
        &self,
        is_client_initiated: bool,
        surface: &WlSurface,
        seat: &Seat<State>,
        serial: impl Into<Option<Serial>>,
        location: Point<i32, Logical>,
        target_stack: bool,
        config: &Config,
        evlh: &LoopHandle<'static, State>,
        dh: &DisplayHandle,
        shell_overlay_state: &mut ShellOverlayState,
        pending_callbacks: &mut std::collections::HashMap<u32, Vec<Item>>,
    ) -> Option<(MenuGrab, Focus)> {
        /// Convert items to protocol representation.
        ///
        /// Walks the tree recursively. `Item::Submenu` becomes a real
        /// `ContextMenuItem::Submenu` (rendered as a fly-out by the shell).
        /// Entries without a `WindowAction` tag fall back to a `Separator`
        /// placeholder so indices in `pending_callbacks` stay aligned with
        /// the DFS order of the protocol stream.
        fn items_to_protocol(items: &[Item]) -> Vec<ContextMenuItem> {
            items
                .iter()
                .map(|item| match item {
                    Item::Separator => ContextMenuItem::Separator,
                    Item::Entry {
                        title,
                        action: Some(action),
                        toggled,
                        disabled,
                        shortcut,
                        ..
                    } => ContextMenuItem::Entry {
                        action: *action,
                        toggled: *toggled,
                        disabled: *disabled,
                        shortcut: shortcut.clone(),
                        // Forward the compositor-side localized title so
                        // the shell doesn't have to re-derive it from
                        // `action`. Non-negotiable for items where many
                        // entries share one `WindowAction` (e.g. the
                        // workspace picker inside "Move to Workspace").
                        label: Some(title.clone()),
                    },
                    Item::Submenu {
                        title,
                        items: children,
                    } => ContextMenuItem::Submenu {
                        label: title.clone(),
                        disabled: false,
                        items: items_to_protocol(children),
                    },
                    Item::Entry { action: None, .. } => {
                        tracing::warn!(
                            "menu_request: item without WindowAction in overlay path; \
                             using Separator placeholder to preserve index alignment"
                        );
                        ContextMenuItem::Separator
                    }
                })
                .collect()
        }

        // `flatten_callbacks` lives in `shell/grabs/menu/mod.rs` so it's
        // covered by unit tests that lock the DFS-index invariant against
        // the Wayland serializer in `ShellOverlayState::send_context_menu`.

        /// Find the desktop-shell's wl_surface by matching the overlay client.
        /// Returns the `PointerFocusTarget` and the surface origin in logical
        /// coordinates (used as pointer-event offset in the grab).
        fn find_shell_focus(
            shell: &Shell,
            shell_overlay_state: &ShellOverlayState,
            dh: &DisplayHandle,
        ) -> Option<(focus::target::PointerFocusTarget, Point<f64, Logical>)> {
            use smithay::reexports::wayland_server::Resource;
            tracing::info!("find_shell_focus: called");
            let Some(instance) = shell_overlay_state.overlay_instance() else {
                tracing::info!("find_shell_focus: no overlay instance, returning None");
                return None;
            };
            let client = instance.client();
            tracing::info!("find_shell_focus: instance client={}", client.is_some());
            let creds = client.and_then(|c| c.get_credentials(dh).ok());
            tracing::info!("find_shell_focus: credentials={creds:?}");
            let Some(overlay_pid) = creds.map(|c| c.pid) else {
                tracing::info!("find_shell_focus: no PID, returning None");
                return None;
            };

            // Desktop-shell is a layer-shell client, not an XDG toplevel.
            // Search the layer map on each output for a surface whose client
            // PID matches the overlay instance PID.
            tracing::info!("find_shell_focus: overlay_pid={overlay_pid}");
            for output in shell.outputs() {
                let layer_map = layer_map_for_output(output);
                let layer_count = layer_map.layers().count();
                tracing::info!("find_shell_focus: output={} layer_count={layer_count}", output.name());
                for layer in layer_map.layers() {
                    let surface = layer.wl_surface();
                    let surface_pid = surface
                        .client()
                        .and_then(|c| c.get_credentials(dh).ok())
                        .map(|creds| creds.pid);
                    tracing::info!("find_shell_focus: layer ns={} pid={surface_pid:?}", layer.namespace());
                    if surface_pid == Some(overlay_pid) {
                        tracing::info!("find_shell_focus: MATCH on layer ns={}", layer.namespace());
                        let target = focus::target::PointerFocusTarget::WlSurface {
                            surface: surface.clone(),
                            toplevel: None,
                        };
                        return Some((target, Point::from((0., 0.))));
                    }
                }
            }
            tracing::info!("find_shell_focus: NO match found");
            None
        }

        let serial = serial.into();
        let Some(GrabStartData::Pointer(start_data)) =
            check_grab_preconditions(seat, serial, is_client_initiated.then_some(surface))
        else {
            return None; // TODO: an application can send a menu request for a touch event
        };

        let items_for_element = |mapped: &CosmicMapped,
                                 is_tiled: bool,
                                 is_sticky: bool,
                                 tiling_enabled: bool,
                                 edge: ResizeEdge,
                                 workspaces: Vec<crate::shell::grabs::menu::WorkspaceMenuEntry>| {
            let is_stacked = mapped.is_stack();

            if target_stack || !is_stacked {
                Box::new(
                    window_items(
                        mapped,
                        is_tiled,
                        is_stacked,
                        is_sticky,
                        tiling_enabled,
                        edge,
                        config,
                        &workspaces,
                    )
                    .collect::<Vec<Item>>()
                    .into_iter(),
                ) as Box<dyn Iterator<Item = Item>>
            } else {
                let (tab, _) = mapped
                    .windows()
                    .find(|(s, _)| s.wl_surface().as_deref() == Some(surface))
                    .unwrap();
                Box::new(
                    tab_items(mapped, &tab, is_tiled, config)
                        .collect::<Vec<Item>>()
                        .into_iter(),
                ) as Box<dyn Iterator<Item = Item>>
            }
        };

        let (global_position, menu_items) = if let Some((set, mapped, relative_loc)) =
            self.workspaces.sets.values().find_map(|set| {
                set.sticky_layer
                    .mapped()
                    .find_map(|m| {
                        m.windows()
                            .find(|(w, _)| w == surface)
                            .map(|(_, loc)| (m, loc))
                    })
                    .map(|(mapped, relative_loc)| (set, mapped, relative_loc))
            }) {
            let output = set.output.clone();
            let global_position = (set.sticky_layer.element_geometry(mapped).unwrap().loc
                + relative_loc.as_local()
                + location.as_local())
            .to_global(&output);
            // Sticky windows aren't on any specific workspace, so the
            // "Move to Workspace N" picker offers every workspace on
            // this output as a potential target (no `is_current`).
            let ws_entries = self
                .workspaces
                .spaces_for_output(&output)
                .enumerate()
                .map(|(i, space)| {
                    crate::shell::grabs::menu::WorkspaceMenuEntry {
                        handle: space.handle,
                        label: space
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("{}", i + 1)),
                        is_current: false,
                    }
                })
                .collect::<Vec<_>>();
            (
                global_position,
                items_for_element(
                    mapped,
                    false,
                    true,
                    false,
                    ResizeEdge::all(),
                    ws_entries,
                ),
            )
        } else if let Some((workspace, output)) = self.workspace_for_surface(surface) {
            let workspace = self.workspaces.space_for_handle(&workspace).unwrap();

            if let Some(window) = workspace.get_fullscreen().filter(|s| *s == surface) {
                let global_position = (workspace.fullscreen_geometry().unwrap().loc
                    + location.as_local())
                .to_global(&output);

                (
                    global_position,
                    Box::new(fullscreen_items(window, config)) as Box<dyn Iterator<Item = Item>>,
                )
            } else {
                let mapped = workspace.element_for_surface(surface)?;
                let elem_geo = workspace.element_geometry(mapped)?;
                let relative_loc = mapped.active_window_geometry().loc;
                let global_position =
                    (elem_geo.loc + relative_loc.as_local() + location.as_local())
                        .to_global(&output);
                let is_tiled = workspace.is_tiled(&mapped.active_window());
                let edge = if is_tiled {
                    mapped
                        .tiling_node_id
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|node_id| {
                            TilingLayout::possible_resizes(workspace.tiling_layer.tree(), node_id)
                        })
                        .unwrap_or(ResizeEdge::empty())
                } else {
                    ResizeEdge::all()
                };

                // Label each workspace on this output; flag the one
                // the window currently lives on so the menu shows it
                // disabled ("you're already there").
                let current_handle = workspace.handle;
                let ws_entries = self
                    .workspaces
                    .spaces_for_output(&output)
                    .enumerate()
                    .map(|(i, space)| {
                        crate::shell::grabs::menu::WorkspaceMenuEntry {
                            handle: space.handle,
                            label: space
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("{}", i + 1)),
                            is_current: space.handle == current_handle,
                        }
                    })
                    .collect::<Vec<_>>();
                (
                    global_position,
                    items_for_element(
                        mapped,
                        is_tiled,
                        false,
                        workspace.tiling_enabled,
                        edge,
                        ws_entries,
                    ),
                )
            }
        } else {
            return None;
        };

        // Collect items so we can both send them over the protocol and pass
        // callbacks to the grab.
        let items: Vec<Item> = menu_items.collect();

        // Try the overlay protocol path first.
        let (menu_id, shell_focus) =
            if let Some(menu_id) = shell_overlay_state.send_context_menu(
                global_position.x,
                global_position.y,
                &items_to_protocol(&items),
            ) {
                let shell_focus = find_shell_focus(self, shell_overlay_state, dh);
                // Callback Vec must be in the same DFS order as the
                // protocol stream so `context_menu_activate(index)` resolves
                // to the right `Item::Entry::on_press` closure, even when
                // submenus nest arbitrarily deep.
                let flat = crate::shell::grabs::menu::flatten_callbacks(&items);
                tracing::info!(
                    "menu_request: menu_id={menu_id} flat callbacks ({} items):",
                    flat.len()
                );
                for (i, it) in flat.iter().enumerate() {
                    match it {
                        Item::Entry { title, action, disabled, .. } => tracing::info!(
                            "  [{i}] Entry title={title:?} action={action:?} disabled={disabled}"
                        ),
                        Item::Submenu { title, .. } => {
                            tracing::info!("  [{i}] Submenu title={title:?}")
                        }
                        Item::Separator => tracing::info!("  [{i}] Separator"),
                    }
                }
                pending_callbacks.insert(menu_id, flat);
                (Some(menu_id), shell_focus)
            } else {
                // No shell client connected - fall back to Iced rendering.
                (None, None)
            };

        let grab = MenuGrab::new(
            GrabStartData::Pointer(start_data),
            seat,
            items.into_iter(),
            global_position,
            MenuAlignment::CORNER,
            None,
            evlh.clone(),
            menu_id,
            shell_focus,
        );

        Some((grab, Focus::Keep))
    }

    pub fn move_request(
        &mut self,
        surface: &WlSurface,
        seat: &Seat<State>,
        serial: impl Into<Option<Serial>>,
        release: ReleaseMode,
        move_out_of_stack: bool,
        config: &Config,
        evlh: &LoopHandle<'static, State>,
        client_initiated: bool,
    ) -> Option<(MoveGrab, Focus)> {
        if self.overview_mode().0.is_active() {
            return None;
        }

        // In monocle mode, windows are fixed (maximized, no dragging).
        let output = seat.active_output();
        if self
            .active_space(&output)
            .is_some_and(|ws| ws.is_monocle())
        {
            return None;
        }

        let serial = serial.into();
        let mut element_geo = None;

        let mut start_data =
            check_grab_preconditions(seat, serial, client_initiated.then_some(surface))?;

        if client_initiated
            && start_data.distance(seat.get_pointer().unwrap().current_location()) < 1.
        {
            return Some((
                MoveGrab::delayed(
                    start_data,
                    surface,
                    seat,
                    serial,
                    release,
                    move_out_of_stack,
                ),
                Focus::Keep,
            ));
        }

        let maybe_fullscreen_workspace = self
            .workspaces
            .spaces_mut()
            .find(|w| w.get_fullscreen().is_some_and(|s| s == surface));
        if let Some(workspace) = maybe_fullscreen_workspace {
            element_geo = Some(workspace.fullscreen_geometry().unwrap());
            let (surface, state, _) = workspace.remove_fullscreen().unwrap();
            self.remap_unfullscreened_window(surface, state, evlh);
        };

        let old_mapped = self.element_for_surface(surface).cloned()?;
        if old_mapped.is_minimized() {
            return None;
        }

        let (window, _) = old_mapped
            .windows()
            .find(|(w, _)| w.wl_surface().as_deref() == Some(surface))
            .unwrap();

        let mapped = if move_out_of_stack {
            let new_mapped: CosmicMapped = CosmicWindow::new(
                window.clone(),
                evlh.clone(),
                self.appearance_conf,
            )
            .into();
            if old_mapped.is_maximized(false) {
                new_mapped.set_maximized(false);
            }
            start_data.set_focus(new_mapped.focus_under((0., 0.).into(), WindowSurfaceType::ALL));
            new_mapped
        } else {
            old_mapped.clone()
        };

        if move_out_of_stack {
            // Update focus stack to set focus to the window being dragged out of
            // the stack.
            if let Some(workspace) = self.space_for_mut(&old_mapped) {
                let mut stack = workspace.focus_stack.get_mut(seat);
                stack.append(mapped.clone());
            }
        }

        let trigger = match &start_data {
            GrabStartData::Pointer(start_data) => Trigger::Pointer(start_data.button),
            GrabStartData::Touch(start_data) => Trigger::Touch(start_data.slot),
        };
        let active_hint = if config.cosmic_conf.active_hint {
            self.lunaris_theme.active_hint as u8
        } else {
            0
        };
        let pointer = seat.get_pointer().unwrap();
        let pos = pointer.current_location().as_global();

        let cursor_output = if let Some(output) = self
            .outputs()
            .find(|output| {
                output
                    .geometry()
                    .as_logical()
                    .overlaps_or_touches(Rectangle::new(
                        start_data.location().to_i32_floor(),
                        (0, 0).into(),
                    ))
            })
            .cloned()
        {
            output
        } else {
            seat.active_output()
        };

        let (initial_window_location, layer, workspace_handle) =
            if let Some(workspace) = self.space_for_mut(&old_mapped) {
                let elem_geo = element_geo.or_else(|| workspace.element_geometry(&old_mapped))?;
                let mut initial_window_location = elem_geo.loc.to_global(workspace.output());

                let mut new_size = if old_mapped.maximized_state.lock().unwrap().is_some() {
                    // If surface is maximized then unmaximize it
                    workspace
                        .unmaximize_request(&old_mapped)
                        .map(|geo| geo.size.as_logical())
                } else {
                    None
                };

                let layer = if if mapped == old_mapped {
                    let was_floating = workspace.floating_layer.unmap(&mapped, None);
                    let was_tiled = workspace
                        .tiling_layer
                        .unmap_as_placeholder(&mapped, PlaceholderType::GrabbedWindow);
                    assert!(was_floating.is_some() != was_tiled.is_some());
                    if was_floating.is_some_and(|geo| geo.size != elem_geo.size) {
                        new_size = was_floating.map(|geo| geo.size.as_logical());
                    }
                    was_tiled.is_some()
                } else {
                    workspace
                        .tiling_layer
                        .mapped()
                        .any(|(m, _)| m == &old_mapped)
                } {
                    ManagedLayer::Tiling
                } else {
                    ManagedLayer::Floating
                };

                // if this changed the width, the window was tiled in floating mode
                if let Some(new_size) = new_size {
                    let output = workspace.output();
                    let ratio = pos.to_local(output).x / (elem_geo.loc.x + elem_geo.size.w) as f64;

                    initial_window_location = Point::from((
                        pos.x - (new_size.w as f64 * ratio),
                        pos.y - MOVE_GRAB_Y_OFFSET,
                    ))
                    .to_i32_round();
                }

                (initial_window_location, layer, workspace.handle)
            } else if let Some(sticky_layer) = self
                .workspaces
                .sets
                .get_mut(&cursor_output)
                .filter(|set| set.sticky_layer.mapped().any(|m| m == &old_mapped))
                .map(|set| &mut set.sticky_layer)
            {
                let elem_geo = sticky_layer.element_geometry(&old_mapped).unwrap();
                let mut initial_window_location = elem_geo.loc.to_global(&cursor_output);

                let mut new_size =
                    if let Some(state) = old_mapped.maximized_state.lock().unwrap().take() {
                        // If surface is maximized then unmaximize it
                        old_mapped.set_maximized(false);
                        let new_size = state.original_geometry.size.as_logical();
                        sticky_layer.map_internal(
                            mapped.clone(),
                            Some(state.original_geometry.loc),
                            Some(new_size),
                            None,
                        );

                        Some(new_size)
                    } else {
                        None
                    };

                if mapped == old_mapped
                    && let Some(geo) = sticky_layer.unmap(&mapped, None)
                    && geo.size != elem_geo.size
                {
                    new_size = Some(geo.size.as_logical());
                }

                if let Some(new_size) = new_size {
                    let ratio =
                        pos.to_local(&cursor_output).x / (elem_geo.loc.x + elem_geo.size.w) as f64;
                    initial_window_location = Point::<f64, _>::from((
                        pos.x - (new_size.w as f64 * ratio),
                        pos.y - MOVE_GRAB_Y_OFFSET,
                    ))
                    .to_i32_round();
                }

                (
                    initial_window_location,
                    ManagedLayer::Sticky,
                    self.active_space(&cursor_output).unwrap().handle,
                )
            } else {
                return None;
            };

        toplevel_leave_workspace(&window, &workspace_handle);
        toplevel_leave_output(&window, &cursor_output);

        if move_out_of_stack {
            old_mapped.stack_ref().unwrap().remove_window(&window);
            self.workspaces
                .space_for_handle_mut(&workspace_handle)
                .unwrap()
                .refresh();
        }

        mapped.set_activate(true);
        mapped.configure();

        let grab = MoveGrab::new(
            start_data,
            mapped,
            seat,
            initial_window_location,
            cursor_output,
            active_hint,
            config.cosmic_conf.edge_snap_threshold as f64,
            layer,
            release,
            evlh.clone(),
        );

        if grab.is_tiling_grab() {
            self.set_overview_mode(Some(trigger), evlh.clone());
        }

        Some((grab, Focus::Clear))
    }

    // Just to avoid a longer lived shell reference
    /// Get the window geometry of a keyboard focus target
    pub fn focused_geometry(&self, target: &KeyboardFocusTarget) -> Option<Rectangle<i32, Global>> {
        match target {
            KeyboardFocusTarget::Fullscreen(surface) => {
                if let Some(workspace) = surface
                    .wl_surface()
                    .and_then(|s| self.workspace_for_surface(&s))
                    .and_then(|(handle, _)| self.workspaces.space_for_handle(&handle))
                {
                    workspace
                        .fullscreen_geometry()
                        .map(|f| f.to_global(workspace.output()))
                } else {
                    None
                }
            }
            _ => {
                if let Some(element) = self.focused_element(target) {
                    self.element_geometry(&element)
                } else {
                    None
                }
            }
        }
    }

    pub fn element_geometry(&self, mapped: &CosmicMapped) -> Option<Rectangle<i32, Global>> {
        if let Some(set) = self
            .workspaces
            .sets
            .values()
            .find(|set| set.sticky_layer.mapped().any(|m| m == mapped))
        {
            let geometry = set
                .sticky_layer
                .element_geometry(mapped)
                .unwrap()
                .to_global(&set.output);
            Some(geometry)
        } else if let Some(workspace) = self.space_for(mapped) {
            let geometry = workspace
                .element_geometry(mapped)
                .unwrap()
                .to_global(workspace.output());
            Some(geometry)
        } else {
            None
        }
    }

    #[must_use]
    pub fn next_focus(&self, direction: FocusDirection, seat: &Seat<State>) -> FocusResult {
        let overview = self.overview_mode().0;
        let Some(target) = seat.get_keyboard().unwrap().current_focus() else {
            return FocusResult::None;
        };
        let output = seat.active_output();

        if matches!(target, KeyboardFocusTarget::Fullscreen(_)) {
            return FocusResult::None;
        }

        let set = self.workspaces.sets.get(&output).unwrap();
        let sticky_layer = &set.sticky_layer;
        let workspace = &set.workspaces[set.active];

        let Some(focused) = (match target {
            KeyboardFocusTarget::Popup(popup) => {
                let Some(toplevel_surface) = (match popup {
                    PopupKind::Xdg(xdg) => get_popup_toplevel(&xdg),
                    PopupKind::InputMethod(_) => unreachable!(),
                }) else {
                    return FocusResult::None;
                };
                sticky_layer
                    .space
                    .elements()
                    .chain(workspace.mapped())
                    .find(|elem| elem.wl_surface().as_deref() == Some(&toplevel_surface))
            }
            KeyboardFocusTarget::Element(elem) => sticky_layer
                .space
                .elements()
                .chain(workspace.mapped())
                .find(|e| *e == &elem),
            KeyboardFocusTarget::Group { .. } => {
                let focus_stack = workspace.focus_stack.get(seat);
                let swap_desc = match overview.active_trigger() {
                    Some(Trigger::KeyboardSwap(_, desc)) => Some(desc.clone()),
                    _ => None,
                };

                return workspace.tiling_layer.next_focus(
                    direction,
                    seat,
                    focus_stack.iter(),
                    swap_desc,
                );
            }
            _ => None,
        })
        .cloned() else {
            return FocusResult::None;
        };

        if focused.handle_focus(seat, direction, None) {
            return FocusResult::Handled;
        }

        // Monocle: cycle through focus stack instead of spatial navigation.
        if workspace.is_monocle() {
            let focus_stack: Vec<_> = workspace
                .focus_stack
                .get(seat)
                .iter()
                .filter_map(|t| match t {
                    FocusTarget::Window(w) => Some(w.clone()),
                    _ => None,
                })
                .collect();

            if focus_stack.len() <= 1 {
                return FocusResult::None;
            }

            let current_idx = focus_stack.iter().position(|w| w == &focused);
            let next_idx = match direction {
                FocusDirection::Down | FocusDirection::Right => {
                    current_idx.map(|i| (i + 1) % focus_stack.len()).unwrap_or(0)
                }
                FocusDirection::Up | FocusDirection::Left => {
                    current_idx
                        .map(|i| if i == 0 { focus_stack.len() - 1 } else { i - 1 })
                        .unwrap_or(0)
                }
                _ => return FocusResult::None,
            };

            return FocusResult::Some(KeyboardFocusTarget::Element(
                focus_stack[next_idx].clone(),
            ));
        }

        if workspace.is_tiled(&focused.active_window()) {
            if focused.is_maximized(false) {
                return FocusResult::None;
            }

            let focus_stack = workspace.focus_stack.get(seat);
            let swap_desc = match overview.active_trigger() {
                Some(Trigger::KeyboardSwap(_, desc)) => Some(desc.clone()),
                _ => None,
            };

            workspace
                .tiling_layer
                .next_focus(direction, seat, focus_stack.iter(), swap_desc)
        } else {
            let floating_layer = &set.workspaces[set.active].floating_layer;

            let geometry = sticky_layer
                .space
                .element_geometry(&focused)
                .or_else(|| floating_layer.space.element_geometry(&focused))
                .unwrap();

            let elements = sticky_layer
                .space
                .elements()
                .chain(floating_layer.space.elements())
                .filter(|elem| *elem != &focused)
                .map(|elem| {
                    (
                        elem,
                        sticky_layer
                            .space
                            .element_geometry(elem)
                            .or_else(|| floating_layer.space.element_geometry(elem))
                            .unwrap(),
                    )
                });

            let next = match direction {
                FocusDirection::Up => elements
                    .filter(|(_, other_geo)| other_geo.loc.y <= geometry.loc.y)
                    .min_by_key(|(_, other_geo)| {
                        let res = geometry.loc.y - other_geo.loc.y;
                        if res.is_positive() { res } else { i32::MAX }
                    }),
                FocusDirection::Down => elements
                    .filter(|(_, other_geo)| other_geo.loc.y > geometry.loc.y)
                    .max_by_key(|(_, other_geo)| {
                        let res = geometry.loc.y - other_geo.loc.y;
                        if res.is_negative() { res } else { i32::MIN }
                    }),
                FocusDirection::Left => elements
                    .filter(|(_, other_geo)| other_geo.loc.x <= geometry.loc.x)
                    .min_by_key(|(_, other_geo)| {
                        let res = geometry.loc.x - other_geo.loc.x;
                        if res.is_positive() { res } else { i32::MAX }
                    }),
                FocusDirection::Right => elements
                    .filter(|(_, other_geo)| other_geo.loc.x > geometry.loc.x)
                    .max_by_key(|(_, other_geo)| {
                        let res = geometry.loc.x - other_geo.loc.x;
                        if res.is_negative() { res } else { i32::MIN }
                    }),
                _ => return FocusResult::None,
            }
            .map(|(other, _)| other);

            next.map(|elem| FocusResult::Some(KeyboardFocusTarget::Element(elem.clone())))
                .unwrap_or(FocusResult::None)
        }
    }

    #[must_use]
    pub fn move_current_element(&mut self, direction: Direction, seat: &Seat<State>) -> MoveResult {
        let Some(output) = seat.focused_output() else {
            return MoveResult::None;
        };
        let workspace = self.active_space(&output).unwrap();
        let focus_stack = workspace.focus_stack.get(seat);
        match focus_stack.last().cloned() {
            Some(FocusTarget::Fullscreen(surface)) => {
                MoveResult::MoveFurther(KeyboardFocusTarget::Fullscreen(surface))
            }
            Some(FocusTarget::Window(mapped)) => {
                if let Some(set) = self
                    .workspaces
                    .sets
                    .values_mut()
                    .find(|set| set.sticky_layer.mapped().any(|m| &mapped == m))
                {
                    set.sticky_layer.move_current_element(
                        direction,
                        seat,
                        ManagedLayer::Sticky,
                    )
                } else {
                    if mapped
                        .maximized_state
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|state| state.original_layer == ManagedLayer::Tiling)
                    {
                        self.unmaximize_request(&mapped);
                    }

                    let workspace = self.active_space_mut(&output).unwrap();
                    workspace
                        .floating_layer
                        .move_current_element(direction, seat, ManagedLayer::Floating)
                        .or_else(|| workspace.tiling_layer.move_current_node(direction, seat))
                }
            }
            _ => MoveResult::None,
        }
    }

    pub fn menu_resize_request(
        &mut self,
        mapped: &CosmicMapped,
        seat: &Seat<State>,
        edge: ResizeEdge,
        edge_snap_threshold: u32,
    ) -> Option<(
        (
            Option<(PointerFocusTarget, Point<f64, Logical>)>,
            Point<i32, Global>,
        ),
        (ResizeGrab, Focus),
    )> {
        if mapped.is_fullscreen(true) || mapped.is_maximized(true) {
            return None;
        }

        let mut start_data = check_grab_preconditions(seat, None, None)?;

        let (floating_layer, geometry) = if let Some(set) = self
            .workspaces
            .sets
            .values_mut()
            .find(|set| set.sticky_layer.mapped().any(|m| m == mapped))
        {
            let geometry = set
                .sticky_layer
                .element_geometry(mapped)
                .unwrap()
                .to_global(&set.output);
            (&mut set.sticky_layer, geometry)
        } else if let Some(workspace) = self.space_for_mut(mapped) {
            let geometry = workspace
                .element_geometry(mapped)
                .unwrap()
                .to_global(workspace.output());
            (&mut workspace.floating_layer, geometry)
        } else {
            return None;
        };

        let new_loc = if edge.contains(ResizeEdge::LEFT) {
            Point::<i32, Global>::from((geometry.loc.x, geometry.loc.y + (geometry.size.h / 2)))
        } else if edge.contains(ResizeEdge::RIGHT) {
            Point::<i32, Global>::from((
                geometry.loc.x + geometry.size.w,
                geometry.loc.y + (geometry.size.h / 2),
            ))
        } else if edge.contains(ResizeEdge::TOP) {
            Point::<i32, Global>::from((geometry.loc.x + (geometry.size.w / 2), geometry.loc.y))
        } else if edge.contains(ResizeEdge::BOTTOM) {
            Point::<i32, Global>::from((
                geometry.loc.x + (geometry.size.w / 2),
                geometry.loc.y + geometry.size.h,
            ))
        } else {
            return None;
        };

        let element_offset = (new_loc - geometry.loc).as_logical();
        let focus = mapped
            .focus_under(element_offset.to_f64(), WindowSurfaceType::ALL)
            .map(|(target, surface_offset)| (target, (surface_offset + element_offset.to_f64())));
        start_data.set_location(new_loc.as_logical().to_f64());
        start_data.set_focus(focus.clone());

        let grab: ResizeGrab = if let Some(grab) = floating_layer.resize_request(
            mapped,
            seat,
            start_data.clone(),
            edge,
            edge_snap_threshold,
            ReleaseMode::Click,
        ) {
            grab.into()
        } else if let Some(ws) = self.space_for_mut(mapped) {
            let node_id = mapped.tiling_node_id.lock().unwrap().clone()?;
            let (node, left_up_idx, orientation) = ws.tiling_layer.resize_request(node_id, edge)?;
            ResizeForkGrab::new(
                start_data,
                new_loc.to_f64(),
                node,
                left_up_idx,
                orientation,
                ws.output.downgrade(),
                ReleaseMode::Click,
            )
            .into()
        } else {
            return None;
        };

        Some(((focus, new_loc), (grab, Focus::Keep)))
    }

    /// Set the tiled-headers enabled state on the shell AND on the
    /// global atomic mirror so render-decision callsites (which
    /// don't have access to a `Shell` borrow) can read it. When
    /// the value actually changes, every tiled mapped window gets
    /// a `configure()` so its `has_ssd` answer flips and the
    /// xdg-toplevel reports the new size to the client. Stack
    /// children are skipped — stacks always have their tab-bar.
    ///
    /// `bump_theme_generation()` is also called so the
    /// per-window header pixmap cache (Feature 4-C) invalidates.
    /// On the next frame `should_render_window_header` returns
    /// the new decision and the window-header element either
    /// appears or disappears atomically with the geometry update.
    pub fn set_tiled_headers_enabled(&mut self, enabled: bool) {
        if self.tiled_headers_enabled == enabled {
            return;
        }
        let old = self.tiled_headers_enabled;
        self.tiled_headers_enabled = enabled;
        crate::shell::set_tiled_headers_global(enabled);
        tracing::info!(
            "TILE-DEBUG tiled_headers_enabled: {} -> {}",
            old, enabled
        );

        // Find every tiled, non-stack mapped window. We collect
        // first to release the iterator borrow on `self` before
        // mutating per-window. Stacks are excluded because they
        // always render their tab-bar header — toggle is a no-op
        // for them.
        // Reconfigure windows actually placed in a tiling layer —
        // floating windows are unaffected by the toggle (their header
        // is gated by CSD only), so reconfiguring them would just
        // cause an unnecessary buffer churn.
        let to_reconfigure: Vec<CosmicMapped> = self
            .mapped()
            .filter(|m| !m.is_stack())
            .filter(|m| m.is_in_tiling_layer())
            .cloned()
            .collect();
        let n = to_reconfigure.len();
        for mapped in &to_reconfigure {
            // `configure()` re-emits xdg_toplevel.configure with
            // current geometry. The client recomputes its content
            // size (now with or without the 36-px SSD strip) and
            // commits a new buffer. The compositor render path
            // re-evaluates `has_ssd` on the next frame.
            mapped.configure();
        }
        tracing::info!(
            "TILE-DEBUG reconfigure storm: {} tiled windows asked to refresh size",
            n
        );

        // Invalidate per-window header pixmap caches so the cached
        // pre-toggle pixmap doesn't paint over the new state.
        crate::backend::render::window_header::bump_theme_generation();
    }

    pub fn maximize_toggle(
        &mut self,
        window: &CosmicMapped,
        seat: &Seat<State>,
        loop_handle: &LoopHandle<'static, State>,
    ) {
        if window.is_maximized(true) {
            self.unmaximize_request(window);
        } else {
            if window.is_fullscreen(true) {
                return;
            }
            self.maximize_request(window, seat, true, loop_handle);
        }
    }

    /// Minimize a window. Returns `Some((window_id, app_id, title,
    /// workspace_id))` when a new minimize actually happened (so the
    /// caller can emit a `window.minimized` event on the event bus).
    /// Returns `None` when the call was a no-op (surface not found, or
    /// already minimized on a sticky layer with no attached workspace).
    ///
    /// The return value is Option-wrapped rather than bool so callers
    /// can emit the event without re-querying the surface's metadata
    /// after it's been moved into the minimized-windows list.
    pub fn minimize_request<S>(&mut self, surface: &S) -> Option<MinimizedInfo>
    where
        CosmicSurface: PartialEq<S>,
    {
        if let Some((set, mapped)) = self.workspaces.sets.values_mut().find_map(|set| {
            let mapped = set
                .sticky_layer
                .mapped()
                .find(|m| &m.active_window() == surface)
                .cloned();
            mapped.map(|m| (set, m))
        }) {
            let to = minimize_rectangle(&set.output, &mapped.active_window());
            let geo = set.sticky_layer.unmap(&mapped, Some(to)).unwrap();
            let active = mapped.active_window();
            // window_id / workspace_id deferred: shell UI uses Wayland
            // toplevel state (not event-bus) so analytics consumers
            // (Knowledge Graph) only need app-level metadata for now.
            let info = MinimizedInfo {
                window_id: String::new(),
                app_id: active.app_id(),
                title: active.title(),
                workspace_id: String::new(),
            };
            set.minimized_windows.push(MinimizedWindow::Floating {
                window: mapped.clone(),
                previous: FloatingRestoreData {
                    geometry: geo,
                    output_size: set.output.geometry().size.as_logical(),
                    was_maximized: false,
                    was_snapped: None,
                },
            });
            return Some(info);
        } else if let Some((workspace, window)) =
            self.workspaces.sets.values_mut().find_map(|set| {
                set.workspaces.iter_mut().find_map(|workspace| {
                    let window = workspace
                        .get_fullscreen()
                        .cloned()
                        .into_iter()
                        .chain(workspace.mapped().map(|m| m.active_window()))
                        .find(|s| s == surface);
                    window.map(|s| (workspace, s))
                })
            })
        {
            let to = minimize_rectangle(workspace.output(), &window);
            let info = MinimizedInfo {
                window_id: String::new(),
                app_id: window.app_id(),
                title: window.title(),
                workspace_id: String::new(),
            };
            if let Some(minimized) = workspace.minimize(surface, to) {
                workspace.minimized_windows.push(minimized);
                return Some(info);
            }
        }
        None
    }

    /// Restore a minimized window. Returns `Some((window_id, app_id))`
    /// when the restore actually happened so callers can emit a
    /// `window.unminimized` event. Returns `None` if the surface wasn't
    /// minimized to begin with.
    pub fn unminimize_request<S>(
        &mut self,
        surface: &S,
        seat: &Seat<State>,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<UnminimizedInfo>
    where
        CosmicSurface: PartialEq<S>,
    {
        if let Some((set, window)) = self.workspaces.sets.values_mut().find_map(|set| {
            set.minimized_windows
                .iter()
                .position(|m| m.windows().any(|s| &s == surface))
                .map(|i| set.minimized_windows.swap_remove(i))
                .map(|window| (set, window))
        }) {
            let MinimizedWindow::Floating { window, previous } = window else {
                unreachable!("None sticky window in WorkspaceSet minimized_windows");
            };

            let from = minimize_rectangle(&set.output, &window.active_window());
            let previous_position =
                previous.position_relative(set.output.geometry().size.as_logical());
            if window.is_stack() {
                window.set_active(surface);
            }
            let active = window.active_window();
            let info = UnminimizedInfo {
                window_id: String::new(),
                app_id: active.app_id(),
            };
            set.sticky_layer
                .remap_minimized(window, from, previous_position);
            Some(info)
        } else {
            let Some((workspace, window)) = self.workspaces.spaces_mut().find_map(|w| {
                w.minimized_windows
                    .iter()
                    .position(|m| m.windows().any(|s| &s == surface))
                    .map(|i| w.minimized_windows.swap_remove(i))
                    .map(|window| (w, window))
            }) else {
                return None;
            };

            if window.mapped().is_some_and(|m| m.is_stack()) {
                window.mapped().unwrap().set_active(surface);
            }
            let active = window.active_window();
            let info = UnminimizedInfo {
                window_id: String::new(),
                app_id: active.app_id(),
            };
            let from = minimize_rectangle(workspace.output(), &window.active_window());
            if let Some((surface, restore, _)) = workspace.unminimize(window, from, seat) {
                toplevel_leave_output(&surface, &workspace.output);
                toplevel_leave_workspace(&surface, &workspace.handle);
                self.remap_unfullscreened_window(surface, restore, loop_handle);
            }
            Some(info)
        }
    }

    pub fn maximize_request(
        &mut self,
        mapped: &CosmicMapped,
        seat: &Seat<State>,
        animate: bool,
        loop_handle: &LoopHandle<'static, State>,
    ) {
        self.unminimize_request(&mapped.active_window(), seat, loop_handle);

        let (original_layer, floating_layer, original_geometry) = if let Some(set) = self
            .workspaces
            .sets
            .values_mut()
            .find(|set| set.sticky_layer.mapped().any(|m| m == mapped))
        {
            let geometry = set.sticky_layer.element_geometry(mapped).unwrap();
            (ManagedLayer::Sticky, &mut set.sticky_layer, geometry)
        } else if let Some(workspace) = self.space_for_mut(mapped) {
            let layer = if workspace.is_tiled(&mapped.active_window()) {
                ManagedLayer::Tiling
            } else {
                ManagedLayer::Floating
            };
            let geometry = workspace.element_geometry(mapped).unwrap();
            (layer, &mut workspace.floating_layer, geometry)
        } else {
            return;
        };

        let mut state = mapped.maximized_state.lock().unwrap();
        if state.is_none() {
            *state = Some(MaximizedState {
                original_geometry,
                original_layer,
            });
            std::mem::drop(state);
            floating_layer.map_maximized(mapped.clone(), original_geometry, animate);
        }
    }

    pub fn unmaximize_request(&mut self, mapped: &CosmicMapped) -> Option<Size<i32, Logical>> {
        if let Some(set) = self.workspaces.sets.values_mut().find(|set| {
            set.sticky_layer.mapped().any(|m| m == mapped)
                || set
                    .minimized_windows
                    .iter()
                    .any(|m| m.mapped().is_some_and(|m| m == mapped))
        }) {
            let mut state = mapped.maximized_state.lock().unwrap();
            if let Some(state) = state.take() {
                assert_eq!(state.original_layer, ManagedLayer::Sticky);

                if let Some(minimized) = set
                    .minimized_windows
                    .iter_mut()
                    .find(|m| m.mapped().is_some_and(|m| m == mapped))
                {
                    minimized.unmaximize(state.original_geometry);
                } else {
                    mapped.set_maximized(false);
                    set.sticky_layer.map_internal(
                        mapped.clone(),
                        Some(state.original_geometry.loc),
                        Some(state.original_geometry.size.as_logical()),
                        None,
                    );
                }
                Some(state.original_geometry.size.as_logical())
            } else {
                None
            }
        } else if let Some(workspace) = self.space_for_mut(mapped) {
            workspace
                .unmaximize_request(mapped)
                .map(|geo| geo.size.as_logical())
        } else {
            None
        }
    }

    pub fn resize_request(
        &mut self,
        surface: &WlSurface,
        seat: &Seat<State>,
        serial: impl Into<Option<Serial>>,
        edges: ResizeEdge,
        edge_snap_threshold: u32,
        client_initiated: bool,
    ) -> Option<(ResizeGrab, Focus)> {
        let serial = serial.into();
        let start_data =
            check_grab_preconditions(seat, serial, client_initiated.then_some(surface))?;
        let mapped = self.element_for_surface(surface).cloned()?;
        if mapped.is_maximized(true) {
            return None;
        }

        let floating_layer = if let Some(set) = self
            .workspaces
            .sets
            .values_mut()
            .find(|set| set.sticky_layer.mapped().any(|m| m == &mapped))
        {
            &mut set.sticky_layer
        } else if let Some(workspace) = self.space_for_mut(&mapped) {
            &mut workspace.floating_layer
        } else {
            return None;
        };

        let grab: ResizeGrab = if let Some(grab) = floating_layer.resize_request(
            &mapped,
            seat,
            start_data.clone(),
            edges,
            edge_snap_threshold,
            ReleaseMode::NoMouseButtons,
        ) {
            grab.into()
        } else if let Some(ws) = self.space_for_mut(&mapped) {
            let node_id = mapped.tiling_node_id.lock().unwrap().clone()?;
            let (node, left_up_idx, orientation) =
                ws.tiling_layer.resize_request(node_id, edges)?;
            ResizeForkGrab::new(
                start_data,
                seat.get_pointer().unwrap().current_location().as_global(),
                node,
                left_up_idx,
                orientation,
                ws.output.downgrade(),
                ReleaseMode::NoMouseButtons,
            )
            .into()
        } else {
            return None;
        };

        Some((grab, Focus::Clear))
    }

    pub fn resize(&mut self, seat: &Seat<State>, direction: ResizeDirection, edge: ResizeEdge) {
        let Some(output) = seat.focused_output() else {
            return;
        };
        let (_, idx) = self.workspaces.active_num(&output);
        let Some(focused) = seat.get_keyboard().unwrap().current_focus() else {
            return;
        };
        let amount = (self
            .resize_state
            .take()
            .map(|(_, _, _, amount, _, _)| amount)
            .unwrap_or(10)
            + 2)
        .min(20);

        if self
            .workspaces
            .sets
            .get_mut(&output)
            .unwrap()
            .sticky_layer
            .resize(&focused, direction, edge, amount)
        {
            self.resize_state = Some((focused, direction, edge, amount, idx, output));
        } else if let Some(workspace) = self.workspaces.get_mut(idx, &output)
            && workspace.resize(&focused, direction, edge, amount)
        {
            self.resize_state = Some((focused, direction, edge, amount, idx, output));
        }
    }

    pub fn finish_resize(&mut self, direction: ResizeDirection, edge: ResizeEdge) {
        if let Some((old_focused, old_direction, old_edge, _, idx, output)) =
            self.resize_state.take()
            && old_direction == direction
            && old_edge == edge
        {
            let Some(toplevel) = old_focused.toplevel() else {
                return;
            };
            let Some(mapped) = self
                .workspaces
                .sets
                .values()
                .find_map(|set| {
                    set.sticky_layer
                        .mapped()
                        .find(|m| m.has_surface(&toplevel, WindowSurfaceType::TOPLEVEL))
                })
                .cloned()
                .or_else(|| {
                    let workspace = self.workspaces.get(idx, &output).unwrap();
                    workspace
                        .mapped()
                        .find(|m| m.has_surface(&toplevel, WindowSurfaceType::TOPLEVEL))
                        .cloned()
                })
            else {
                return;
            };

            let mut resize_state = mapped.resize_state.lock().unwrap();
            if let Some(ResizeState::Resizing(data)) = *resize_state {
                mapped.set_resizing(false);
                *resize_state = Some(ResizeState::WaitingForCommit(data));
            }
        }
    }

    #[must_use]
    pub fn toggle_stacking(
        &mut self,
        seat: &Seat<State>,
        window: &CosmicMapped,
    ) -> Option<KeyboardFocusTarget> {
        if let Some(set) = self
            .workspaces
            .sets
            .values_mut()
            .find(|set| set.sticky_layer.mapped().any(|m| m == window))
        {
            let workspace = &mut set.workspaces[set.active];
            set.sticky_layer
                .toggle_stacking(window, workspace.focus_stack.get_mut(seat))
        } else if let Some(workspace) = self.space_for_mut(window) {
            if workspace.tiling_layer.mapped().any(|(m, _)| m == window) {
                workspace
                    .tiling_layer
                    .toggle_stacking(window, workspace.focus_stack.get_mut(seat))
            } else if workspace.floating_layer.mapped().any(|w| w == window) {
                workspace
                    .floating_layer
                    .toggle_stacking(window, workspace.focus_stack.get_mut(seat))
            } else {
                None
            }
        } else {
            None
        }
    }

    #[must_use]
    pub fn toggle_stacking_focused(
        &mut self,
        seat: &Seat<State>,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget> {
        let focused_output = seat.focused_output()?;
        let set = self.workspaces.sets.get_mut(&focused_output).unwrap();
        let workspace = &mut set.workspaces[set.active];

        if matches!(
            seat.get_keyboard().unwrap().current_focus(),
            Some(KeyboardFocusTarget::Fullscreen(_))
        ) {
            return None;
        }

        let maybe_window = workspace.focus_stack.get(seat).iter().next().cloned();
        if let Some(FocusTarget::Window(window)) = maybe_window {
            let was_maximized = window.is_maximized(false);
            if was_maximized {
                workspace.unmaximize_request(&window);
            }

            let res = if set.sticky_layer.mapped().any(|m| m == &window) {
                set.sticky_layer
                    .toggle_stacking_focused(seat, workspace.focus_stack.get_mut(seat))
            } else if workspace.tiling_layer.mapped().any(|(m, _)| m == &window) {
                workspace
                    .tiling_layer
                    .toggle_stacking_focused(seat, workspace.focus_stack.get_mut(seat))
            } else if workspace.floating_layer.mapped().any(|w| w == &window) {
                workspace
                    .floating_layer
                    .toggle_stacking_focused(seat, workspace.focus_stack.get_mut(seat))
            } else {
                None
            };

            if was_maximized && let Some(KeyboardFocusTarget::Element(mapped)) = res.as_ref() {
                self.maximize_request(mapped, seat, false, loop_handle);
            }

            res
        } else {
            None
        }
    }

    pub fn toggle_sticky(&mut self, seat: &Seat<State>, mapped: &CosmicMapped) {
        // clean from focus-stacks
        for workspace in self.workspaces.spaces_mut() {
            for seat in self.seats.iter() {
                let mut stack = workspace.focus_stack.get_mut(seat);
                stack.remove(mapped);
            }
        }

        if let Some(workspace) = self.space_for_mut(mapped) {
            let previous_layer = if workspace.is_tiled(&mapped.active_window()) {
                workspace.toggle_floating_window(seat, mapped);
                ManagedLayer::Tiling
            } else {
                ManagedLayer::Floating
            };
            let Some(geometry) = workspace.element_geometry(mapped) else {
                return;
            };
            workspace.unmap_element(mapped);

            *mapped.previous_layer.lock().unwrap() = Some(previous_layer);
            let output = workspace.output().clone();
            let handle = workspace.handle;

            for (window, _) in mapped.windows() {
                window.set_sticky(true);
                toplevel_leave_workspace(&window, &handle);
            }

            let set = self.workspaces.sets.get_mut(&output).unwrap();
            set.sticky_layer.map(mapped.clone(), geometry.loc);

            let mut state = mapped.maximized_state.lock().unwrap();
            if let Some(MaximizedState {
                original_geometry,
                original_layer: _,
            }) = *state
            {
                *state = Some(MaximizedState {
                    original_geometry,
                    original_layer: ManagedLayer::Sticky,
                });
                std::mem::drop(state);
                set.workspaces[set.active].floating_layer.map_maximized(
                    mapped.clone(),
                    geometry,
                    false,
                );
            }
        } else if let Some(set) = self
            .workspaces
            .sets
            .values_mut()
            .find(|set| set.sticky_layer.mapped().any(|m| m == mapped))
        {
            let geometry = set.sticky_layer.unmap(mapped, None).unwrap();

            let workspace = &mut set.workspaces[set.active];
            for (window, _) in mapped.windows() {
                toplevel_enter_workspace(&window, &workspace.handle);
                window.set_sticky(false);
            }
            let previous_layer = mapped
                .previous_layer
                .lock()
                .unwrap()
                .take()
                .unwrap_or(ManagedLayer::Floating);

            match previous_layer {
                ManagedLayer::Tiling if workspace.tiling_enabled => {
                    let focus_stack = workspace.focus_stack.get(seat);
                    workspace
                        .tiling_layer
                        .map(mapped.clone(), Some(focus_stack.iter()), None);
                }
                ManagedLayer::Sticky => unreachable!(),
                _ => workspace.floating_layer.map(mapped.clone(), geometry.loc),
            }

            let mut state = mapped.maximized_state.lock().unwrap();
            if let Some(MaximizedState {
                original_geometry,
                original_layer: _,
            }) = *state
            {
                *state = Some(MaximizedState {
                    original_geometry,
                    original_layer: previous_layer,
                });
                std::mem::drop(state);
                workspace
                    .floating_layer
                    .map_maximized(mapped.clone(), geometry, false);
            }
        }

        self.append_focus_stack(mapped.clone(), seat);
    }

    pub fn toggle_sticky_current(&mut self, seat: &Seat<State>) {
        if matches!(
            seat.get_keyboard().unwrap().current_focus(),
            Some(KeyboardFocusTarget::Fullscreen(_))
        ) {
            return;
        }
        let set = self.workspaces.sets.get_mut(&seat.active_output()).unwrap();
        let workspace = &mut set.workspaces[set.active];

        let maybe_window = workspace.focus_stack.get(seat).iter().next().cloned();
        if let Some(FocusTarget::Window(mapped)) = maybe_window {
            self.toggle_sticky(seat, &mapped);
        }
    }

    #[must_use]
    pub fn fullscreen_request<S>(
        &mut self,
        surface: &S,
        output: Output,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget>
    where
        CosmicSurface: PartialEq<S>,
    {
        let mapped = self.element_for_surface(surface).cloned()?;
        let window;

        let old_fullscreen = if let Some((old_output, set)) = self
            .workspaces
            .sets
            .iter_mut()
            .find(|(_, set)| set.sticky_layer.mapped().any(|m| m == &mapped))
        {
            let mut from = set.sticky_layer.element_geometry(&mapped).unwrap();
            let mut was_maximized = false;
            window = if mapped
                .stack_ref()
                .map(|stack| stack.len() > 1)
                .unwrap_or(false)
            {
                let stack = mapped.stack_ref().unwrap();
                let surface = stack.surfaces().find(|s| s == surface).unwrap();
                stack.remove_window(&surface);
                surface
            } else {
                // Must be set before `map_internal`/`unmap` below, as both may call
                // intermediate `configure()`, which would send a configure event without the
                // fullscreen state, causing clients like Chromium to cancel the transition.
                mapped.set_fullscreen(true);

                if let Some(state) = mapped.maximized_state.lock().unwrap().take() {
                    mapped.set_maximized(false);
                    set.sticky_layer.map_internal(
                        mapped.clone(),
                        Some(state.original_geometry.loc),
                        Some(state.original_geometry.size.as_logical()),
                        None,
                    );
                    was_maximized = true;
                }

                from = set.sticky_layer.unmap(&mapped, None).unwrap();
                mapped.active_window()
            };

            toplevel_leave_output(&window, old_output);
            let old_output = old_output.downgrade();
            let workspace_handle = self.active_space(&output).unwrap().handle;
            toplevel_enter_output(&window, &output);
            toplevel_enter_workspace(&window, &workspace_handle);

            let workspace = self.active_space_mut(&output).unwrap();
            workspace.map_fullscreen(
                &window,
                None,
                Some(FullscreenRestoreState::Sticky {
                    output: old_output,
                    state: FloatingRestoreData {
                        geometry: from,
                        output_size: workspace.output.geometry().size.as_logical(),
                        was_maximized,
                        was_snapped: None,
                    },
                }),
                Some(from),
            )
        } else if let Some(workspace) = self.space_for_mut(&mapped) {
            if mapped.is_minimized() {
                // TODO: Rewrite the `MinimizedWindow` to restore to fullscreen
                return None;
            }

            // Must be set before `unmap_surface()`.
            // `Workspace::unmap_surface` may call intermediate `configure()` internally, which would send
            // a configure event without the fullscreen state, causing clients like Chromium to cancel the transition.
            mapped.set_fullscreen(true);

            let from = workspace.element_geometry(&mapped).unwrap();
            let (surface, state) = workspace.unmap_surface(surface).unwrap();
            window = surface;
            let handle = workspace.handle;

            toplevel_leave_output(&window, &workspace.output);
            toplevel_leave_workspace(&window, &workspace.handle);

            let workspace = self.active_space_mut(&output).unwrap();
            toplevel_enter_output(&window, &output);
            toplevel_enter_workspace(&window, &workspace.handle);

            workspace.map_fullscreen(
                &window,
                None,
                match state {
                    WorkspaceRestoreData::Floating(floating_state) => {
                        floating_state.map(|state| FullscreenRestoreState::Floating {
                            workspace: handle,
                            state,
                        })
                    }
                    WorkspaceRestoreData::Tiling(tiling_state) => {
                        tiling_state.map(|state| FullscreenRestoreState::Tiling {
                            workspace: handle,
                            state,
                        })
                    }
                    WorkspaceRestoreData::Fullscreen(_) => unreachable!(),
                },
                Some(from),
            )
        } else {
            return None;
        };

        if let Some((old_fullscreen, restore, _)) = old_fullscreen {
            self.remap_unfullscreened_window(old_fullscreen, restore, loop_handle);
        }

        Some(KeyboardFocusTarget::Fullscreen(window))
    }

    pub fn unfullscreen_request<S>(
        &mut self,
        surface: &S,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget>
    where
        CosmicSurface: PartialEq<S>,
    {
        let maybe_workspace = self.workspaces.iter_mut().find_map(|(_, s)| {
            s.workspaces
                .iter_mut()
                .find(|w| w.get_fullscreen().is_some_and(|f| f == surface))
        });

        if let Some(workspace) = maybe_workspace {
            let (old_fullscreen, restore, _) = workspace.remove_fullscreen().unwrap();
            toplevel_leave_output(&old_fullscreen, &workspace.output);
            toplevel_leave_workspace(&old_fullscreen, &workspace.handle);

            let window = self.remap_unfullscreened_window(old_fullscreen, restore, loop_handle);
            Some(KeyboardFocusTarget::Element(window))
        } else {
            None
        }
    }

    pub fn set_theme(
        &mut self,
        xdg_activation_state: &XdgActivationState,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        self.refresh(xdg_activation_state, workspace_state);
        self.workspaces.set_theme();
    }

    pub fn update_tiling_exceptions<'a, I>(&mut self, exceptions: I)
    where
        I: Iterator<Item = &'a ApplicationException>,
    {
        self.tiling_exceptions = layout::TilingExceptions::new(exceptions);
    }

    pub fn take_presentation_feedback(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut output_presentation_feedback = OutputPresentationFeedback::new(output);

        if let Some(active) = self.active_space(output) {
            active.mapped().for_each(|mapped| {
                mapped.active_window().take_presentation_feedback(
                    &mut output_presentation_feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(
                            surface,
                            render_element_states,
                        )
                    },
                );
            });
        }

        self.override_redirect_windows.iter().for_each(|or| {
            if let Some(wl_surface) = or.wl_surface() {
                take_presentation_feedback_surface_tree(
                    &wl_surface,
                    &mut output_presentation_feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(
                            surface,
                            render_element_states,
                        )
                    },
                )
            }
        });

        let map = smithay::desktop::layer_map_for_output(output);
        for layer_surface in map.layers() {
            layer_surface.take_presentation_feedback(
                &mut output_presentation_feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }

        output_presentation_feedback
    }

    pub fn mapped(&self) -> impl Iterator<Item = &CosmicMapped> {
        self.workspaces.iter().flat_map(|(_, set)| {
            set.sticky_layer
                .mapped()
                .chain(set.minimized_windows.iter().flat_map(|m| m.mapped()))
                .chain(set.workspaces.iter().flat_map(|w| {
                    w.mapped()
                        .chain(w.minimized_windows.iter().flat_map(|m| m.mapped()))
                }))
        })
    }

    // ── Scratchpad ───────────────────────────────────────────────────────

    /// Move the focused window to the scratchpad.
    ///
    /// Removes the window from its current layer (tiling or floating),
    /// hides it, and adds it to the scratchpad list.
    pub fn scratchpad_move(&mut self, seat: &Seat<State>) {
        let output = seat.focused_or_active_output();
        let Some(workspace) = self.active_space_mut(&output) else {
            return;
        };

        // Get the focused mapped element.
        let maybe_window = workspace.focus_stack.get(seat).iter().next().cloned();
        let Some(FocusTarget::Window(mapped)) = maybe_window else {
            return;
        };

        // Remove from workspace (handles both tiling and floating).
        workspace.unmap_element(&mapped);

        // Hide all surfaces of the mapped element.
        for (surface, _) in mapped.windows() {
            if let Some(wl) = surface.wl_surface() {
                // Setting geometry to zero effectively hides the window.
                // The window is kept alive in the scratchpad list.
            }
        }

        tracing::info!(
            "scratchpad: moved {} to scratchpad (now {} windows)",
            mapped.active_window().app_id(),
            self.scratchpad.windows.len() + 1,
        );

        self.scratchpad.windows.push(mapped);
    }

    /// Toggle scratchpad visibility.
    ///
    /// - If no scratchpad window is visible and scratchpad is not empty:
    ///   show the first window (floating, centered, 80% output size).
    /// - If a scratchpad window is visible and focused: hide it.
    /// - If a scratchpad window is visible but not focused: focus it.
    /// - If pressed again while focused: cycle to next scratchpad window.
    pub fn scratchpad_toggle(&mut self, seat: &Seat<State>) {
        if self.scratchpad.windows.is_empty() {
            return;
        }

        let output = seat.focused_or_active_output();

        if let Some(visible_idx) = self.scratchpad.visible {
            // A scratchpad window is currently shown.
            let mapped = &self.scratchpad.windows[visible_idx];

            // Check if the scratchpad window is currently focused.
            let Some(workspace) = self.active_space(&output) else {
                return;
            };
            let is_focused = workspace
                .focus_stack
                .get(seat)
                .iter()
                .next()
                .is_some_and(|f| matches!(f, FocusTarget::Window(w) if w == mapped));

            if is_focused {
                // Already focused: cycle to next or hide.
                let next_idx = (visible_idx + 1) % self.scratchpad.windows.len();
                if next_idx == visible_idx {
                    // Only one window: hide it.
                    self.scratchpad_hide(seat);
                } else {
                    // Hide current, show next.
                    self.scratchpad_hide(seat);
                    self.scratchpad_show(next_idx, seat);
                }
            } else {
                // Visible but not focused: just focus it.
                // The window is already in the floating layer.
            }
        } else {
            // No scratchpad window visible: show the first one.
            self.scratchpad_show(0, seat);
        }
    }

    /// Show a scratchpad window by index.
    ///
    /// Maps it to the active workspace's floating layer, centered at 80%
    /// of the output size.
    fn scratchpad_show(&mut self, idx: usize, seat: &Seat<State>) {
        if idx >= self.scratchpad.windows.len() {
            return;
        }

        let output = seat.focused_or_active_output();
        let output_geo = output.geometry();

        // 80% of output size.
        let width = (output_geo.size.w as f64 * 0.8) as i32;
        let height = (output_geo.size.h as f64 * 0.8) as i32;
        let x = output_geo.loc.x + (output_geo.size.w - width) / 2;
        let y = output_geo.loc.y + (output_geo.size.h - height) / 2;

        let mapped = self.scratchpad.windows[idx].clone();

        // Configure the window size.
        mapped.set_geometry(smithay::utils::Rectangle::from_loc_and_size(
            (0, 0),
            (width, height),
        ));

        let Some(workspace) = self.active_space_mut(&output) else {
            return;
        };

        // Map to floating layer at centered position.
        let local_pos = Point::<i32, Logical>::from((
            x - output_geo.loc.x,
            y - output_geo.loc.y,
        )).as_local();
        workspace.floating_layer.map(mapped, Some(local_pos));

        self.scratchpad.visible = Some(idx);

        tracing::info!("scratchpad: showing window {idx}");
    }

    /// Hide the currently visible scratchpad window.
    fn scratchpad_hide(&mut self, seat: &Seat<State>) {
        let Some(visible_idx) = self.scratchpad.visible.take() else {
            return;
        };

        if visible_idx >= self.scratchpad.windows.len() {
            return;
        }

        let mapped = self.scratchpad.windows[visible_idx].clone();
        let output = seat.focused_or_active_output();

        if let Some(workspace) = self.active_space_mut(&output) {
            workspace.unmap_element(&mapped);
        }

        tracing::info!("scratchpad: hiding window {visible_idx}");
    }

    /// Remove a window from the scratchpad if it was closed.
    ///
    /// Called during surface cleanup to handle the edge case where
    /// a scratchpad window is destroyed.
    pub fn scratchpad_remove_dead(&mut self) {
        let before = self.scratchpad.windows.len();
        self.scratchpad.windows.retain(|w| w.alive());

        if self.scratchpad.windows.len() != before {
            // Adjust visible index.
            if let Some(idx) = self.scratchpad.visible {
                if idx >= self.scratchpad.windows.len() {
                    self.scratchpad.visible = None;
                }
            }
        }
    }
}

fn workspace_set_idx(
    state: &mut WorkspaceUpdateGuard<'_, State>,
    idx: u8,
    handle: &WorkspaceHandle,
) {
    state.set_workspace_name(handle, format!("{}", idx));
    state.set_workspace_coordinates(handle, &[idx as u32]);
}

pub fn check_grab_preconditions(
    seat: &Seat<State>,
    serial: Option<Serial>,
    client_initiated: Option<&WlSurface>,
) -> Option<GrabStartData> {
    use smithay::reexports::wayland_server::Resource;

    let pointer = seat.get_pointer().unwrap();
    let touch = seat.get_touch().unwrap();

    let start_data =
        if serial.is_some_and(|serial| touch.has_grab(serial)) {
            GrabStartData::Touch(touch.grab_start_data().unwrap())
        } else {
            GrabStartData::Pointer(pointer.grab_start_data().unwrap_or_else(|| {
                PointerGrabStartData {
                    focus: pointer.current_focus().map(|f| (f, Point::from((0., 0.)))),
                    button: 0x110,
                    location: pointer.current_location(),
                }
            }))
        };

    if let Some(surface) = client_initiated {
        // Check that this surface has a click or touch down grab.
        if !match serial {
            Some(serial) => pointer.has_grab(serial) || touch.has_grab(serial),
            None => pointer.is_grabbed() | touch.is_grabbed(),
        } {
            return None;
        }

        // If the focus was for a different surface, ignore the request.
        if start_data.focus().is_none()
            || !start_data.focus().unwrap().0.same_client_as(&surface.id())
        {
            return None;
        }
    }

    Some(start_data)
}

#[cfg(test)]
mod tiled_headers_policy_tests {
    use super::{HeaderEligibilityInputs, header_eligibility_pure,
        TILED_HEADERS_ENABLED, set_tiled_headers_global, tiled_headers_enabled_global};
    use std::sync::atomic::Ordering;

    fn inputs(
        is_stack: bool,
        is_fullscreen: bool,
        is_tiled: bool,
        tiled_headers_enabled: bool,
        csd: bool,
    ) -> HeaderEligibilityInputs {
        HeaderEligibilityInputs {
            is_stack,
            is_fullscreen,
            is_tiled,
            tiled_headers_enabled,
            csd_branch_eligible: csd,
        }
    }

    #[test]
    fn stack_always_renders_regardless_of_other_inputs() {
        // Stack short-circuits everything — fullscreen, tiled+off,
        // CSD-only client. Tab-bar is functional UI, must always be
        // visible while the stack exists.
        for fs in [true, false] {
            for tiled in [true, false] {
                for toggle in [true, false] {
                    for csd in [true, false] {
                        let i = inputs(true, fs, tiled, toggle, csd);
                        assert!(
                            header_eligibility_pure(i),
                            "stack should render: {:?}",
                            i
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fullscreen_never_renders() {
        // Outside the stack short-circuit, fullscreen always wins.
        for tiled in [true, false] {
            for toggle in [true, false] {
                for csd in [true, false] {
                    let i = inputs(false, true, tiled, toggle, csd);
                    assert!(!header_eligibility_pure(i), "fullscreen: {:?}", i);
                }
            }
        }
    }

    #[test]
    fn tiled_with_toggle_off_does_not_render() {
        // Default scenario the toggle was added for: tiled SSD
        // window (Kitty in tiling mode) gets no header.
        let i = inputs(false, false, true, false, true);
        assert!(!header_eligibility_pure(i));
    }

    #[test]
    fn tiled_with_toggle_on_renders_when_csd_branch_says_yes() {
        let i = inputs(false, false, true, true, true);
        assert!(header_eligibility_pure(i));
    }

    #[test]
    fn floating_renders_regardless_of_toggle() {
        // Floating windows IGNORE the toggle — they always show
        // their decoration if the CSD branch says so. This is
        // critical for "floating window in tiling workspace via
        // window-rule" scenarios.
        for toggle in [true, false] {
            let i = inputs(false, false, false, toggle, true);
            assert!(header_eligibility_pure(i), "floating ignores toggle: toggle={toggle}");
        }
    }

    #[test]
    fn floating_with_csd_client_never_renders() {
        // Client-side-decorated app (Firefox, GTK4) — Lunaris
        // never paints chrome regardless of toggle.
        for toggle in [true, false] {
            let i = inputs(false, false, false, toggle, false);
            assert!(!header_eligibility_pure(i), "csd app never gets header: toggle={toggle}");
        }
    }

    #[test]
    fn tiled_with_csd_client_never_renders_either() {
        // Tiled CSD app: header not from us regardless. Toggle off
        // is the relevant case here — even if toggle were on, no
        // header (because csd_branch_eligible=false).
        let i = inputs(false, false, true, true, false);
        assert!(!header_eligibility_pure(i));
        let i = inputs(false, false, true, false, false);
        assert!(!header_eligibility_pure(i));
    }

    // ── Global atomic mirror ──

    #[test]
    fn global_toggle_defaults_to_false() {
        // Reading the static at any time before the first set
        // should give us false. (This test sets+resets to be
        // robust against test-runner ordering.)
        let saved = TILED_HEADERS_ENABLED.load(Ordering::Relaxed);
        TILED_HEADERS_ENABLED.store(false, Ordering::Relaxed);
        assert!(!tiled_headers_enabled_global());
        // Restore so other tests don't see stale state.
        TILED_HEADERS_ENABLED.store(saved, Ordering::Relaxed);
    }

    #[test]
    fn set_tiled_headers_global_round_trip() {
        let saved = TILED_HEADERS_ENABLED.load(Ordering::Relaxed);
        set_tiled_headers_global(true);
        assert!(tiled_headers_enabled_global());
        set_tiled_headers_global(false);
        assert!(!tiled_headers_enabled_global());
        TILED_HEADERS_ENABLED.store(saved, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod header_id_tests {
    use super::HEADER_ID_X11_NAMESPACE;

    /// Simulate the Wayland encoding: take a protocol id and mask the
    /// top bit off. Real Wayland client-allocated ids never use the
    /// top bit anyway, but the mask is our safety net.
    fn encode_wayland(protocol_id: u32) -> u32 {
        protocol_id & !HEADER_ID_X11_NAMESPACE
    }

    /// Simulate the X11 encoding: take an XID and set the top bit.
    fn encode_x11(xid: u32) -> u32 {
        xid | HEADER_ID_X11_NAMESPACE
    }

    #[test]
    fn wayland_encoding_clears_top_bit() {
        assert_eq!(encode_wayland(0x0000_0002), 0x0000_0002);
        assert_eq!(encode_wayland(0x007F_FFFF), 0x007F_FFFF);
        assert_eq!(encode_wayland(0x0123_4567), 0x0123_4567);
    }

    #[test]
    fn x11_encoding_sets_top_bit() {
        assert_eq!(encode_x11(0x0020_0000), 0x8020_0000);
        assert_eq!(encode_x11(0x0040_0042), 0x8040_0042);
    }

    #[test]
    fn namespace_top_bit_is_reserved_for_x11() {
        // Real protocol ids and real X11 XIDs, encoded, must land in
        // distinct halves. Verify on a handful of realistic values
        // (Wayland starts at 2, X11 XIDs begin around 0x200000).
        let wl_ids: [u32; 4] = [0x02, 0xff, 0x00f0_0001, 0x1234];
        let x11_ids: [u32; 4] = [0x0020_0000, 0x0040_0002, 0x00a0_0010, 0x0f00_0001];
        for id in wl_ids.iter().copied() {
            assert!(encode_wayland(id) & HEADER_ID_X11_NAMESPACE == 0);
        }
        for id in x11_ids.iter().copied() {
            assert!(encode_x11(id) & HEADER_ID_X11_NAMESPACE != 0);
        }
    }

    #[test]
    fn encodings_are_round_trippable() {
        // Given an encoded id, we can tell whether it's Wayland or
        // X11 by inspecting the top bit — that's how the action
        // resolver will dispatch.
        let wl_encoded = encode_wayland(0x1234);
        let x11_encoded = encode_x11(0x0020_1234);
        assert_eq!(wl_encoded & HEADER_ID_X11_NAMESPACE, 0);
        assert_eq!(x11_encoded & HEADER_ID_X11_NAMESPACE, HEADER_ID_X11_NAMESPACE);
        // Recover the X11 XID by masking.
        assert_eq!(x11_encoded & !HEADER_ID_X11_NAMESPACE, 0x0020_1234);
    }
}
