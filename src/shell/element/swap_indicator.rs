use crate::utils::iced::{IcedElement, Program};

use calloop::LoopHandle;
use smithay::utils::Size;

pub type SwapIndicator = IcedElement<SwapIndicatorInternal>;

pub fn swap_indicator(
    evlh: LoopHandle<'static, crate::state::State>,
    theme: cosmic::Theme,
) -> SwapIndicator {
    SwapIndicator::new(SwapIndicatorInternal, Size::from((1, 1)), evlh, theme)
}

pub struct SwapIndicatorInternal;

impl Program for SwapIndicatorInternal {
    type Message = ();

    fn view(&self) -> cosmic::Element<'_, Self::Message> {
        cosmic::iced::widget::row(Vec::new()).into()
    }
}
