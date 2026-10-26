use iced::theme::Palette;
use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector, color};

pub const BACKDROP: Color = color!(0x0d1016);
pub const SURFACE: Color = color!(0x151a23);
pub const SURFACE_RAISED: Color = color!(0x1c222d);
pub const ROW_SELECTED: Color = color!(0x222a38);
pub const BORDER: Color = color!(0x2a3140);
pub const TEXT: Color = color!(0xe4e8ef);
pub const TEXT_MUTED: Color = color!(0x8a93a3);
pub const TEXT_FAINT: Color = color!(0x5d6575);
pub const ACCENT: Color = color!(0x5b8def);
pub const ACCENT_SOFT: Color = color!(0x2d4a7c);
pub const SUCCESS: Color = color!(0x6cc28a);
pub const WARNING: Color = color!(0xe0b04a);
pub const DANGER: Color = color!(0xd9655b);
pub const DANGER_SOFT: Color = color!(0x5a2a2a);
pub const MARK_BG: Color = color!(0x6b5516);
pub const MARK_TEXT: Color = color!(0xffe6a3);
pub const CLIPBOARD_DOT: Color = color!(0xb48cf0);

pub const RADIUS: f32 = 10.0;

const APP_PALETTE: [Color; 10] = [
    color!(0x5b8def),
    color!(0xd98c4a),
    color!(0x6aa874),
    color!(0xcf6680),
    color!(0x9b7bd4),
    color!(0x4aaeae),
    color!(0xcbb24a),
    color!(0xc95b5b),
    color!(0x5ab0dc),
    color!(0x8cba4c),
];

pub fn theme() -> Theme {
    Theme::custom(
        "Capricorn",
        Palette {
            background: BACKDROP,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            warning: WARNING,
            danger: DANGER,
        },
    )
}

/// Same class, same colour, across runs and machines.
pub fn app_colour(class: &str) -> Color {
    APP_PALETTE[app_colour_index(class)]
}

fn app_colour_index(class: &str) -> usize {
    let hash = class
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    (hash % APP_PALETTE.len() as u64) as usize
}

pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn panel(_: &Theme) -> container::Style {
    container::Style {
        background: Some(SURFACE.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn backdrop(_: &Theme) -> container::Style {
    container::Style {
        background: Some(BACKDROP.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn fill(colour: Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(colour.into()),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

pub fn divider(_: &Theme) -> container::Style {
    container::Style {
        background: Some(BORDER.into()),
        ..container::Style::default()
    }
}

pub fn pill(foreground: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(SURFACE_RAISED.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 999.0.into(),
        },
        text_color: Some(foreground),
        ..container::Style::default()
    }
}

pub fn chip(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = match (active, hovered) {
            (true, _) => ACCENT_SOFT,
            (false, true) => ROW_SELECTED,
            (false, false) => SURFACE_RAISED,
        };
        button::Style {
            background: Some(background.into()),
            text_color: if active { TEXT } else { TEXT_MUTED },
            border: Border {
                color: if active { ACCENT } else { BORDER },
                width: 1.0,
                radius: 999.0.into(),
            },
            ..button::Style::default()
        }
    }
}

pub fn chip_flat(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: (active || hovered).then_some(Background::Color(ROW_SELECTED)),
            text_color: TEXT,
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let background = (selected || hovered).then_some(Background::Color(ROW_SELECTED));
        button::Style {
            background,
            text_color: TEXT,
            border: Border::default(),
            ..button::Style::default()
        }
    }
}

pub fn action(tone: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let alpha = match status {
            button::Status::Hovered | button::Status::Pressed => 1.0,
            button::Status::Disabled => 0.4,
            button::Status::Active => 0.85,
        };
        button::Style {
            background: Some(Color { a: alpha, ..tone }.into()),
            text_color: TEXT,
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then_some(Background::Color(ROW_SELECTED)),
        text_color: TEXT,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Borderless input that sits inside a header row.
pub fn bare_input(_: &Theme, _: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: TEXT_MUTED,
        placeholder: TEXT_FAINT,
        value: TEXT,
        selection: ACCENT_SOFT,
    }
}

pub fn boxed_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(BACKDROP),
        border: Border {
            color: if focused { ACCENT } else { BORDER },
            width: 1.0,
            radius: 8.0.into(),
        },
        icon: TEXT_MUTED,
        placeholder: TEXT_FAINT,
        value: TEXT,
        selection: ACCENT_SOFT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_colour_stable() {
        assert_eq!(app_colour("kitty"), app_colour("kitty"));
        assert_eq!(app_colour("Firefox"), app_colour("firefox"));
        let indices = ["kitty", "firefox", "vesktop", "neovim"].map(app_colour_index);
        assert_eq!(indices, [2, 4, 5, 9]);
        assert!(indices.iter().all(|&i| i < APP_PALETTE.len()));
    }
}
