use std::sync::Mutex;

use crate::{
    shell::grabs::ResizeEdge,
    utils::iced::{IcedElement, Program},
};

use calloop::LoopHandle;
use cosmic_settings_config::shortcuts::action::ResizeDirection;
use smithay::utils::Size;

pub type ResizeIndicator = IcedElement<ResizeIndicatorInternal>;

pub fn resize_indicator(
    direction: ResizeDirection,
    config: &crate::config::Config,
    evlh: LoopHandle<'static, crate::state::State>,
    theme: cosmic::Theme,
) -> ResizeIndicator {
    use cosmic_settings_config::shortcuts::action::Action;
    ResizeIndicator::new(
        ResizeIndicatorInternal {
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
        },
        Size::from((1, 1)),
        evlh,
        theme,
    )
}

pub struct ResizeIndicatorInternal {
    pub edges: Mutex<ResizeEdge>,
    pub direction: ResizeDirection,
    pub shortcut1: String,
    pub shortcut2: String,
}

impl Program for ResizeIndicatorInternal {
    type Message = ();

    fn view(&self) -> cosmic::Element<'_, Self::Message> {
        cosmic::iced::widget::row(Vec::new()).into()
    }
}
