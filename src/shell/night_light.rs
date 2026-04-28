/// Night light gamma engine.
///
/// Warms the screen by routing a per-channel multiply through the
/// existing offscreen postprocess shader (`offscreen.frag`).
/// The pipeline is:
///
/// 1. `kelvin_to_rgb` converts a target colour temperature in
///    Kelvin to a `(red, green, blue)` triple in `[0.0, 1.0]` using
///    the Tanner-Helland approximation (same curve gammastep /
///    redshift use).
/// 2. `apply_to_backend` writes the triple into
///    `ScreenFilter::night_light_tint`, pushes the filter into the
///    backend's per-output `ScreenFilterStorage`, and schedules a
///    redraw on every output. The shader picks up the new uniform
///    on the next frame and multiplies the final pixel.
///
/// Backend-uniform: the shader path works on every backend
/// (Winit, X11, KMS), avoiding the split-brain failure mode where
/// one monitor would tint via DRM hardware gamma and another would
/// stay cold because its CRTC has no programmable LUT. We
/// considered using `drm::set_gamma` as a perf optimization on
/// KMS, but a single fullscreen multiplicative shader pass costs
/// well under a millisecond on any GPU we care about, so the
/// uniformity is worth more than the optimization.

use std::time::{Duration, Instant};

use calloop::{LoopHandle, timer::{TimeoutAction, Timer}};
use tracing::{info, warn};

use crate::state::State;

/// Schedule-timer cadence. 60s is the published spec — fast enough
/// that boundary crossings are not visibly delayed, slow enough
/// that solar-position recomputation is essentially free.
const SCHEDULE_TICK: Duration = Duration::from_secs(60);

/// Lowest temperature the engine will accept. Below ~1000K the
/// approximation breaks down and the screen looks black; cap to
/// keep the slider safe.
pub const MIN_TEMPERATURE_K: u16 = 1000;
/// Daylight neutral. Above this we clamp because warming makes no
/// sense (we'd be cooling, which is what blue-light filters
/// already avoid).
pub const MAX_TEMPERATURE_K: u16 = 6500;

/// User-facing schedule for night light. Determines when the engine
/// transitions to the warm temperature and back to neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NightLightSchedule {
    /// Manual on/off — `enabled` flag is the source of truth.
    Manual,
    /// Sunset to sunrise based on the user's `[location]`. Computed
    /// in the schedule timer (D2.3); the engine itself is
    /// schedule-agnostic.
    SunsetSunrise,
    /// Custom window in minutes-since-midnight. The schedule timer
    /// flips `enabled` at the boundaries.
    Custom { start_min: u32, end_min: u32 },
}

impl Default for NightLightSchedule {
    fn default() -> Self {
        NightLightSchedule::Manual
    }
}

/// Long-lived night-light state hung off `Common`. The schedule
/// timer mutates `enabled` and the apply requests mutate
/// `target_temperature_k`; `current_temperature_k` is the value
/// the engine has actually written to hardware so smooth
/// transitions can lerp between the two.
#[derive(Debug, Clone)]
pub struct NightLightState {
    /// Whether night light is currently active. When `false`, the
    /// engine drives the LUT toward `NEUTRAL_TEMPERATURE_K` (6500K),
    /// which is an identity ramp.
    pub enabled: bool,
    /// User's intent for the manual flag. The schedule timer holds
    /// `enabled` to whatever the schedule says, so a separate field
    /// is needed to remember "the user toggled it on" across schedule
    /// transitions. The shell sets this on every `set_night_light`
    /// request; the schedule evaluator consults it for `Manual` mode.
    pub manual_state: bool,
    /// Target colour temperature when enabled. User-controlled via
    /// the Settings slider / Quick Settings toggle.
    pub target_temperature_k: u16,
    /// Last temperature actually written to hardware. Used by the
    /// smooth-transition path to lerp; absent that, snapped to the
    /// target on each apply.
    pub current_temperature_k: u16,
    /// Active schedule. Mutated by `set_night_light_schedule`.
    pub schedule: NightLightSchedule,
    /// User location for the `SunsetSunrise` schedule mode. Stored
    /// as f64 degrees (the shell-side request encodes as signed
    /// micro-degrees and we decode here). `(0.0, 0.0)` means unset.
    pub location: crate::shell::night_light_schedule::Location,
    /// Wall-clock time of the last apply. Useful for diagnostics
    /// and for future rate-limiting (DRM gamma writes are not free).
    pub last_apply: Option<Instant>,
}

const NEUTRAL_TEMPERATURE_K: u16 = 6500;
const DEFAULT_WARM_TEMPERATURE_K: u16 = 3400;

impl Default for NightLightState {
    fn default() -> Self {
        Self {
            enabled: false,
            manual_state: false,
            target_temperature_k: DEFAULT_WARM_TEMPERATURE_K,
            current_temperature_k: NEUTRAL_TEMPERATURE_K,
            schedule: NightLightSchedule::default(),
            location: crate::shell::night_light_schedule::Location::default(),
            last_apply: None,
        }
    }
}

impl NightLightState {
    /// The temperature the engine *should* be displaying right now,
    /// taking the enabled flag into account. `enabled=false` always
    /// means neutral 6500K (= identity LUT, no tint).
    pub fn effective_temperature_k(&self) -> u16 {
        if self.enabled {
            self.target_temperature_k
                .clamp(MIN_TEMPERATURE_K, MAX_TEMPERATURE_K)
        } else {
            NEUTRAL_TEMPERATURE_K
        }
    }
}

/// Convert a colour temperature in Kelvin to an `(r, g, b)` triple
/// in `[0.0, 1.0]` per channel. Implementation of the Tanner-Helland
/// approximation. Above 6500K the tint goes blue-cool, below 1000K
/// it collapses to red — the engine clamps before calling so we
/// stay in the warm-tint regime.
///
/// References:
/// - <https://tannerhelland.com/2012/09/18/convert-temperature-rgb-algorithm-code.html>
/// - gammastep / redshift use the same curve.
pub fn kelvin_to_rgb(temperature_k: u16) -> (f32, f32, f32) {
    let t = (temperature_k as f32) / 100.0;

    let red = if t <= 66.0 {
        255.0
    } else {
        (329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2)).clamp(0.0, 255.0)
    };

    let green = if t <= 66.0 {
        (99.470_802_586_1 * t.ln() - 161.119_568_166_1).clamp(0.0, 255.0)
    } else {
        (288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)).clamp(0.0, 255.0)
    };

    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        (138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7).clamp(0.0, 255.0)
    };

    (red / 255.0, green / 255.0, blue / 255.0)
}

/// Install the recurring 60s schedule timer on the event loop. The
/// timer evaluates the active `NightLightSchedule` against the
/// current local time, flips `enabled` if needed, and re-applies
/// the gamma LUT to the backend. Manual schedule mode is a no-op
/// path through the same code (the user toggle stays sticky).
///
/// Called once during startup, after `init_backend_auto`, so the
/// first tick has a real backend to write to.
pub fn install_schedule_timer(handle: &LoopHandle<'static, State>) {
    use chrono::Local;

    let token = handle.insert_source(Timer::from_duration(SCHEDULE_TICK), |_, _, state| {
        let nl = &state.common.night_light_state;
        let target = crate::shell::night_light_schedule::evaluate(
            nl.schedule,
            nl.manual_state,
            Local::now(),
            nl.location,
        );
        if target != state.common.night_light_state.enabled {
            info!(
                from = state.common.night_light_state.enabled,
                to = target,
                schedule = ?state.common.night_light_state.schedule,
                "night_light: schedule transition",
            );
            state.common.night_light_state.enabled = target;
            apply_to_backend(state);
        }
        TimeoutAction::ToDuration(SCHEDULE_TICK)
    });
    if let Err(err) = token {
        warn!(?err, "night_light: failed to install schedule timer");
    }
}

/// Apply the engine's current effective state via the offscreen
/// postprocess shader.
///
/// The path is the same on every backend: write the per-channel
/// Kelvin ratio into `ScreenFilter::night_light_tint`, push the
/// filter into the backend's per-output `ScreenFilterStorage`, and
/// schedule a redraw on every output so the next frame picks up
/// the new uniform.
///
/// We deliberately do NOT take the DRM `set_gamma` shortcut on
/// KMS. It would split-brain on multi-monitor setups where one
/// CRTC has a programmable LUT (warmed via hardware) and another
/// does not (left cold because the global "fallback off" flag was
/// already set). Adversarial review caught this; uniform shader
/// is the safer default. The shader pass costs ~one fragment per
/// output pixel, well under a millisecond on any GPU we ship.
pub fn apply_to_backend(state: &mut State) {
    let temp = state.common.night_light_state.effective_temperature_k();
    let neutral = !state.common.night_light_state.enabled || temp == 6500;
    let tint: Option<[f32; 3]> = if neutral {
        None
    } else {
        let (r, g, b) = kelvin_to_rgb(temp);
        Some([r, g, b])
    };

    {
        let mut filter = state.common.config.dynamic_conf.screen_filter_mut();
        filter.night_light_tint = tint;
    }
    let filter_snapshot = state.common.config.dynamic_conf.screen_filter().clone();
    if let Err(err) = state.backend.update_screen_filter(&filter_snapshot) {
        warn!(?err, "night_light: backend screen-filter update failed");
    }

    state.common.night_light_state.current_temperature_k = temp;
    state.common.night_light_state.last_apply = Some(Instant::now());

    // Force a redraw on every active output so the new tint is
    // visible without waiting for the next natural damage event.
    let outputs: Vec<_> = state.common.shell.read().outputs().cloned().collect();
    for output in outputs {
        state.backend.schedule_render(&output);
    }

    info!(
        enabled = state.common.night_light_state.enabled,
        temp_k = temp,
        shader_active = tint.is_some(),
        "night_light: applied",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kelvin_neutral_returns_full_channels() {
        let (r, g, b) = kelvin_to_rgb(NEUTRAL_TEMPERATURE_K);
        // Neutral is just below the t=66 threshold (6500/100=65), so
        // green is curved but red is saturated and blue is on the
        // ascent. We allow a small slack on green/blue because the
        // approximation is not perfectly 1.0 at exactly 6500K — it's
        // a few percent off, which matches the original algorithm.
        assert!((r - 1.0).abs() < 1e-3, "red {r}");
        assert!(g > 0.95, "green {g}");
        assert!(b > 0.85, "blue {b}");
    }

    #[test]
    fn kelvin_warm_drops_blue() {
        let (_r, _g, b_warm) = kelvin_to_rgb(3400);
        let (_, _, b_neutral) = kelvin_to_rgb(NEUTRAL_TEMPERATURE_K);
        assert!(b_warm < b_neutral, "warm blue {b_warm} >= neutral {b_neutral}");
        assert!(b_warm < 0.7, "warm blue {b_warm} not strongly warmed");
    }

    #[test]
    fn kelvin_very_warm_collapses_blue() {
        let (_, _, b) = kelvin_to_rgb(1500);
        assert!(b < 0.05, "1500K blue {b} not near zero");
    }

    #[test]
    fn effective_temperature_when_disabled_is_neutral() {
        let mut s = NightLightState::default();
        s.enabled = false;
        s.target_temperature_k = 3000;
        assert_eq!(s.effective_temperature_k(), NEUTRAL_TEMPERATURE_K);
    }

    #[test]
    fn effective_temperature_clamps_to_range() {
        let mut s = NightLightState::default();
        s.enabled = true;
        s.target_temperature_k = 500;
        assert_eq!(s.effective_temperature_k(), MIN_TEMPERATURE_K);
        s.target_temperature_k = 9000;
        assert_eq!(s.effective_temperature_k(), MAX_TEMPERATURE_K);
    }
}
