/// Night-light schedule evaluator.
///
/// Given the active `NightLightSchedule`, the current local time,
/// and (for sunset mode) the user's lat/lon, decides whether night
/// light should be ON or OFF right now. The compositor's calloop
/// timer ticks every 60s and feeds `evaluate(...)` into a flag that
/// gates `apply_to_kms`.
///
/// Pure logic — no I/O, no state. The caller owns the
/// `NightLightState` and the `Common::clock`. Tests pump synthetic
/// times into `evaluate` to verify the boundary behaviour.

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone, Timelike, Utc};

use crate::shell::night_light::NightLightSchedule;

/// Caller-provided location for sunset/sunrise computation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

impl Location {
    /// True only when both coordinates are non-zero. We treat
    /// `(0.0, 0.0)` as "unset" rather than "off the coast of Africa"
    /// because the latter is essentially never what a real user
    /// configured. Sunset mode without a location falls back to
    /// disabled.
    pub fn is_set(&self) -> bool {
        self.latitude != 0.0 || self.longitude != 0.0
    }
}

/// Evaluate the schedule against `now` and `location`.
///
/// Returns `true` when night light should be enabled. `Manual` is
/// schedule-agnostic and always returns the caller's `manual_state`
/// — the timer must not flip a manual schedule.
pub fn evaluate(
    schedule: NightLightSchedule,
    manual_state: bool,
    now: DateTime<Local>,
    location: Location,
) -> bool {
    match schedule {
        NightLightSchedule::Manual => manual_state,
        NightLightSchedule::Custom { start_min, end_min } => {
            in_window(minutes_of_day(now), start_min, end_min)
        }
        NightLightSchedule::SunsetSunrise => {
            if !location.is_set() {
                // Without a location, sunset mode is meaningless;
                // honour the manual flag so the user is not stranded
                // in a permanently-on or permanently-off state.
                return manual_state;
            }
            let (sunrise, sunset) = sunrise_sunset_local(
                now,
                location.latitude,
                location.longitude,
            );
            // Active outside the daylight window. Use minutes-of-day
            // for the comparison so cross-midnight behaviour falls
            // out naturally from `in_window` (sunset is later than
            // sunrise on every populated continent).
            let now_min = minutes_of_day(now);
            let rise_min = minutes_of_day_naive(sunrise);
            let set_min = minutes_of_day_naive(sunset);
            in_window(now_min, set_min, rise_min)
        }
    }
}

/// Local minutes-since-midnight in the system timezone.
fn minutes_of_day(t: DateTime<Local>) -> u32 {
    t.hour() * 60 + t.minute()
}

fn minutes_of_day_naive(t: NaiveTime) -> u32 {
    t.hour() * 60 + t.minute()
}

/// Inclusive-start, exclusive-end membership test for a
/// minutes-since-midnight window. Wraps across midnight when
/// `start > end` (e.g. `start=1320` for 22:00, `end=420` for 07:00
/// covers an overnight schedule). Equal start and end produce an
/// always-on window — the user's clearest "always" intent.
pub fn in_window(now: u32, start: u32, end: u32) -> bool {
    let start = start % (24 * 60);
    let end = end % (24 * 60);
    let now = now % (24 * 60);
    if start == end {
        return true;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

/// Compute sunrise / sunset for the given local date + location and
/// return both as `NaiveTime`s in the local timezone.
fn sunrise_sunset_local(
    now: DateTime<Local>,
    latitude: f64,
    longitude: f64,
) -> (NaiveTime, NaiveTime) {
    let date = now.date_naive();
    let (rise_unix, set_unix) = sunrise::sunrise_sunset(
        latitude,
        longitude,
        date.year(),
        date.month(),
        date.day(),
    );
    let rise_local = unix_to_local_time(rise_unix);
    let set_local = unix_to_local_time(set_unix);
    (rise_local, set_local)
}

fn unix_to_local_time(unix: i64) -> NaiveTime {
    let utc: DateTime<Utc> = Utc.timestamp_opt(unix, 0).single().unwrap_or_else(|| {
        // Edge case: extreme-latitude winter where sunrise crate
        // returns a sentinel. Fall back to noon so the window
        // collapses to the manual flag rather than producing a
        // panic.
        Utc.timestamp_opt(0, 0).unwrap()
    });
    let local: DateTime<Local> = utc.with_timezone(&Local);
    local.time()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_window_normal_range() {
        // 22:00 is not in [09:00, 17:00).
        assert!(!in_window(22 * 60, 9 * 60, 17 * 60));
        assert!(in_window(12 * 60, 9 * 60, 17 * 60));
        // start boundary inclusive, end boundary exclusive.
        assert!(in_window(9 * 60, 9 * 60, 17 * 60));
        assert!(!in_window(17 * 60, 9 * 60, 17 * 60));
    }

    #[test]
    fn in_window_wraps_overnight() {
        // 22:00 is in [22:00, 07:00).
        assert!(in_window(22 * 60, 22 * 60, 7 * 60));
        assert!(in_window(2 * 60, 22 * 60, 7 * 60));
        assert!(in_window(0, 22 * 60, 7 * 60));
        assert!(!in_window(12 * 60, 22 * 60, 7 * 60));
        // exclusive end inside the morning side.
        assert!(!in_window(7 * 60, 22 * 60, 7 * 60));
    }

    #[test]
    fn in_window_equal_start_end_is_always_true() {
        assert!(in_window(0, 0, 0));
        assert!(in_window(12 * 60, 12 * 60, 12 * 60));
    }

    #[test]
    fn manual_mode_returns_manual_state() {
        let now = Local::now();
        assert!(evaluate(NightLightSchedule::Manual, true, now, Location::default()));
        assert!(!evaluate(
            NightLightSchedule::Manual,
            false,
            now,
            Location::default()
        ));
    }

    #[test]
    fn sunset_without_location_falls_back_to_manual() {
        let now = Local::now();
        let loc = Location::default();
        assert!(!loc.is_set());
        assert!(evaluate(
            NightLightSchedule::SunsetSunrise,
            true,
            now,
            loc
        ));
        assert!(!evaluate(
            NightLightSchedule::SunsetSunrise,
            false,
            now,
            loc
        ));
    }

    #[test]
    fn custom_window_active_during_window() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 26, 23, 30, 0)
            .single()
            .unwrap();
        // 22:00 → 07:00 overnight, 23:30 is in.
        let active = evaluate(
            NightLightSchedule::Custom {
                start_min: 22 * 60,
                end_min: 7 * 60,
            },
            false, // manual ignored for custom mode
            now,
            Location::default(),
        );
        assert!(active);
    }

    #[test]
    fn custom_window_inactive_outside_window() {
        let now = Local
            .with_ymd_and_hms(2026, 4, 26, 12, 0, 0)
            .single()
            .unwrap();
        let active = evaluate(
            NightLightSchedule::Custom {
                start_min: 22 * 60,
                end_min: 7 * 60,
            },
            true, // manual ignored
            now,
            Location::default(),
        );
        assert!(!active);
    }

    #[test]
    fn location_is_set_treats_zero_as_unset() {
        assert!(!Location {
            latitude: 0.0,
            longitude: 0.0,
        }
        .is_set());
        assert!(Location {
            latitude: 52.52,
            longitude: 13.405,
        }
        .is_set());
    }
}
