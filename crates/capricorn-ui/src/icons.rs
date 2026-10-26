use iced::widget::{Svg, svg};
use iced::{Color, Length};

/// Lucide icons (ISC licence, see `assets/icons/LICENSE-lucide`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    Lock,
    LockOpen,
    Clipboard,
    Monitor,
    Copy,
    Reopen,
    Trash,
    Calendar,
    Close,
    ChevronLeft,
    ChevronRight,
    Enter,
}

impl Icon {
    fn bytes(self) -> &'static [u8] {
        match self {
            Icon::Search => include_bytes!("../assets/icons/search.svg"),
            Icon::Lock => include_bytes!("../assets/icons/lock.svg"),
            Icon::LockOpen => include_bytes!("../assets/icons/lock-open.svg"),
            Icon::Clipboard => include_bytes!("../assets/icons/clipboard.svg"),
            Icon::Monitor => include_bytes!("../assets/icons/monitor.svg"),
            Icon::Copy => include_bytes!("../assets/icons/copy.svg"),
            Icon::Reopen => include_bytes!("../assets/icons/rotate-ccw.svg"),
            Icon::Trash => include_bytes!("../assets/icons/trash.svg"),
            Icon::Calendar => include_bytes!("../assets/icons/calendar.svg"),
            Icon::Close => include_bytes!("../assets/icons/x.svg"),
            Icon::ChevronLeft => include_bytes!("../assets/icons/chevron-left.svg"),
            Icon::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Icon::Enter => include_bytes!("../assets/icons/corner-down-left.svg"),
        }
    }
}

pub fn icon<'a>(icon: Icon, size: f32, colour: Color) -> Svg<'a> {
    svg(svg::Handle::from_memory(icon.bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_, _| svg::Style {
            color: Some(colour),
        })
}
