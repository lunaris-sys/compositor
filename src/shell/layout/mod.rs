// SPDX-License-Identifier: GPL-3.0-only

use cosmic_settings_config::{shortcuts::action::Orientation, window_rules::ApplicationException};
use regex::{Regex, RegexSet};
use smithay::{
    desktop::WindowSurface,
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
    xwayland::xwm::WmWindowType,
};
use tracing::warn;

use super::CosmicSurface;

pub mod floating;
pub mod tiling;

pub fn is_dialog(window: &CosmicSurface) -> bool {
    // Check "window type"
    match window.0.underlying_surface() {
        WindowSurface::Wayland(toplevel) => {
            if with_states(toplevel.wl_surface(), |states| {
                let attrs = states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap();
                attrs.parent.is_some()
            }) {
                return true;
            }
        }
        WindowSurface::X11(surface) => {
            if surface.is_override_redirect()
                || surface.is_popup()
                || !matches!(
                    surface.window_type(),
                    None | Some(WmWindowType::Normal) | Some(WmWindowType::Utility)
                )
            {
                return true;
            }
        }
    };

    // Check if sizing suggest dialog
    let max_size = window.max_size_without_ssd();
    let min_size = window.min_size_without_ssd();

    if min_size.is_some() && min_size == max_size {
        return true;
    }

    false
}

#[derive(Debug, Clone, Default)]
pub struct TilingExceptions {
    app_ids: RegexSet,
    titles: RegexSet,
}

impl TilingExceptions {
    pub fn new<'a, I>(exceptions_config: I) -> Self
    where
        I: Iterator<Item = &'a ApplicationException>,
    {
        let mut app_ids = Vec::new();
        let mut titles = Vec::new();

        for exception in exceptions_config {
            if let Err(e) = Regex::new(&exception.appid) {
                warn!("Invalid regex for appid: {}, {}", exception.appid, e);
                continue;
            }
            if let Err(e) = Regex::new(&exception.title) {
                warn!("Invalid regex for title: {}, {}", exception.appid, e);
                continue;
            }

            app_ids.push(exception.appid.clone());
            titles.push(exception.title.clone());
        }

        Self {
            app_ids: RegexSet::new(app_ids).unwrap(),
            titles: RegexSet::new(titles).unwrap(),
        }
    }
}

pub fn has_floating_exception(exceptions: &TilingExceptions, window: &CosmicSurface) -> bool {
    // Check cosmic-config exceptions (legacy).
    let appid_matches = exceptions.app_ids.matches(&window.app_id());
    let title_matches = exceptions.titles.matches(&window.title());
    for idx in appid_matches.into_iter() {
        if title_matches.matched(idx) {
            return true;
        }
    }

    false
}

/// Check TOML window rules for float/tile action.
///
/// Returns `Some(true)` for force-float, `Some(false)` for force-tile,
/// `None` if no rule matches (fall through to default behavior).
pub fn check_window_rules(
    rules: &[crate::config::WindowRule],
    window: &CosmicSurface,
) -> Option<bool> {
    let app_id = window.app_id();
    let title = window.title();
    let dialog = is_dialog(window);

    for rule in rules {
        if rule.matcher.matches(&app_id, &title, dialog) {
            return Some(rule.action == crate::config::WindowAction::Float);
        }
    }
    None
}
