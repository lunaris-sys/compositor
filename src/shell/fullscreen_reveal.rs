/// Fullscreen titlebar edge-reveal state machine.
///
/// Detects pointer at the top screen edge during fullscreen and drives
/// reveal/hide events sent to the desktop shell via `lunaris-shell-overlay`.
///
/// Timing (from `docs/architecture/titlebar-protocol.md`):
/// - Pointer enters top edge (y <= 2px): immediate reveal
/// - Pointer leaves titlebar area: 300ms delay, then hide
/// - Debounce: rapid enter/leave within 50ms cancels pending hide

use std::time::{Duration, Instant};

/// Height of the edge-detection zone in logical pixels.
const EDGE_THRESHOLD: f64 = 2.0;

/// Height of the titlebar area in logical pixels (36px top bar).
const TITLEBAR_HEIGHT: f64 = 36.0;

/// Delay before hiding the titlebar after pointer leaves.
const HIDE_DELAY: Duration = Duration::from_millis(300);

/// Debounce window: re-entering within this time cancels a pending hide.
const DEBOUNCE: Duration = Duration::from_millis(50);

/// Reveal state machine phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealPhase {
    /// Titlebar hidden, no pointer at edge.
    Hidden,
    /// Titlebar visible (reveal event sent).
    Visible,
    /// Pointer left titlebar, waiting for hide delay.
    HidePending,
}

/// Per-output fullscreen reveal state.
#[derive(Debug)]
pub struct FullscreenRevealState {
    pub phase: RevealPhase,
    /// Surface ID of the current fullscreen window (0 = none).
    pub surface_id: u32,
    /// When the hide delay started (only valid in HidePending).
    pub hide_timer: Option<Instant>,
    /// Last time pointer re-entered the edge zone (for debounce).
    last_enter: Option<Instant>,
}

impl Default for FullscreenRevealState {
    fn default() -> Self {
        Self {
            phase: RevealPhase::Hidden,
            surface_id: 0,
            hide_timer: None,
            last_enter: None,
        }
    }
}

/// Result of a state machine tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealAction {
    /// No change, do nothing.
    None,
    /// Send `fullscreen_titlebar_reveal` to the shell.
    Reveal,
    /// Send `fullscreen_titlebar_hide` to the shell.
    Hide,
}

impl FullscreenRevealState {
    /// Update the state machine with the current pointer Y position.
    ///
    /// `pointer_y` is the pointer position relative to the output top edge
    /// (0.0 = top of screen). `has_fullscreen` indicates whether a fullscreen
    /// window is active on this output. `fullscreen_surface_id` is the
    /// wl_surface protocol ID of the fullscreen window.
    ///
    /// Returns the action the caller should take.
    pub fn update(
        &mut self,
        pointer_y: f64,
        has_fullscreen: bool,
        fullscreen_surface_id: u32,
    ) -> RevealAction {
        // No fullscreen window: reset to hidden.
        if !has_fullscreen {
            if self.phase != RevealPhase::Hidden {
                let old_phase = self.phase;
                self.reset();
                if old_phase == RevealPhase::Visible || old_phase == RevealPhase::HidePending {
                    return RevealAction::Hide;
                }
            }
            return RevealAction::None;
        }

        // Fullscreen window changed: reset state.
        if fullscreen_surface_id != self.surface_id {
            let was_visible =
                self.phase == RevealPhase::Visible || self.phase == RevealPhase::HidePending;
            self.reset();
            self.surface_id = fullscreen_surface_id;
            if was_visible {
                return RevealAction::Hide;
            }
        }

        let in_edge = pointer_y <= EDGE_THRESHOLD;
        let in_titlebar = pointer_y <= TITLEBAR_HEIGHT;
        let now = Instant::now();

        match self.phase {
            RevealPhase::Hidden => {
                if in_edge {
                    self.phase = RevealPhase::Visible;
                    self.last_enter = Some(now);
                    self.hide_timer = None;
                    return RevealAction::Reveal;
                }
                RevealAction::None
            }
            RevealPhase::Visible => {
                if !in_titlebar {
                    // Pointer left the titlebar area: start hide delay.
                    self.phase = RevealPhase::HidePending;
                    self.hide_timer = Some(now);
                }
                RevealAction::None
            }
            RevealPhase::HidePending => {
                if in_titlebar {
                    // Pointer came back into titlebar: cancel hide.
                    self.phase = RevealPhase::Visible;
                    self.hide_timer = None;
                    self.last_enter = Some(now);

                    // If we were debouncing (re-entered quickly), no event needed.
                    // The shell still thinks titlebar is visible.
                    return RevealAction::None;
                }

                // Check if hide delay has elapsed.
                if let Some(timer_start) = self.hide_timer {
                    // Debounce: if the last enter was very recent, extend the delay.
                    let effective_start = if let Some(last) = self.last_enter {
                        if now.duration_since(last) < DEBOUNCE {
                            // Re-entered and left very quickly: extend timer.
                            now
                        } else {
                            timer_start
                        }
                    } else {
                        timer_start
                    };

                    if now.duration_since(effective_start) >= HIDE_DELAY {
                        self.reset();
                        return RevealAction::Hide;
                    }
                }
                RevealAction::None
            }
        }
    }

    /// Reset to initial hidden state.
    fn reset(&mut self) {
        self.phase = RevealPhase::Hidden;
        self.surface_id = 0;
        self.hide_timer = None;
        self.last_enter = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_no_fullscreen_stays_hidden() {
        let mut state = FullscreenRevealState::default();
        assert_eq!(state.update(0.0, false, 0), RevealAction::None);
        assert_eq!(state.phase, RevealPhase::Hidden);
    }

    #[test]
    fn test_edge_triggers_reveal() {
        let mut state = FullscreenRevealState::default();
        assert_eq!(state.update(1.0, true, 42), RevealAction::Reveal);
        assert_eq!(state.phase, RevealPhase::Visible);
        assert_eq!(state.surface_id, 42);
    }

    #[test]
    fn test_pointer_in_titlebar_stays_visible() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        assert_eq!(state.update(20.0, true, 42), RevealAction::None);
        assert_eq!(state.phase, RevealPhase::Visible);
    }

    #[test]
    fn test_pointer_leaves_starts_hide_pending() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        assert_eq!(state.update(50.0, true, 42), RevealAction::None);
        assert_eq!(state.phase, RevealPhase::HidePending);
    }

    #[test]
    fn test_pointer_returns_cancels_hide() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        state.update(50.0, true, 42); // leave
        assert_eq!(state.update(10.0, true, 42), RevealAction::None);
        assert_eq!(state.phase, RevealPhase::Visible);
    }

    #[test]
    fn test_hide_after_delay() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        state.update(50.0, true, 42); // leave
        // Simulate time passing (both timer and last_enter in the past).
        let past = Instant::now() - HIDE_DELAY - Duration::from_millis(100);
        state.hide_timer = Some(past);
        state.last_enter = Some(past);
        assert_eq!(state.update(50.0, true, 42), RevealAction::Hide);
        assert_eq!(state.phase, RevealPhase::Hidden);
    }

    #[test]
    fn test_fullscreen_gone_hides() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        assert_eq!(state.phase, RevealPhase::Visible);
        assert_eq!(state.update(1.0, false, 0), RevealAction::Hide);
        assert_eq!(state.phase, RevealPhase::Hidden);
    }

    #[test]
    fn test_surface_change_resets() {
        let mut state = FullscreenRevealState::default();
        state.update(1.0, true, 42);
        // Different fullscreen window.
        assert_eq!(state.update(1.0, true, 99), RevealAction::Hide);
        // Edge triggers reveal for new surface.
        // (The hide happened, now we need a new edge detection.)
        assert_eq!(state.phase, RevealPhase::Hidden);
    }

    #[test]
    fn test_below_edge_no_reveal() {
        let mut state = FullscreenRevealState::default();
        assert_eq!(state.update(10.0, true, 42), RevealAction::None);
        assert_eq!(state.phase, RevealPhase::Hidden);
    }
}
