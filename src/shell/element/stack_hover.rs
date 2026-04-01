use crate::utils::iced::{IcedElement, Program};

use calloop::LoopHandle;
use smithay::utils::{Logical, Size};

pub type StackHover = IcedElement<StackHoverInternal>;

pub fn stack_hover(
    evlh: LoopHandle<'static, crate::state::State>,
    size: Size<i32, Logical>,
    theme: cosmic::Theme,
) -> StackHover {
    StackHover::new(StackHoverInternal, size, evlh, theme)
}

pub struct StackHoverInternal;

impl Program for StackHoverInternal {
    type Message = ();

    fn view(&self) -> cosmic::Element<'_, Self::Message> {
        cosmic::iced::widget::row(Vec::new()).into()
    }
}
