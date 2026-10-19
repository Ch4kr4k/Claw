//! The app's look: Solarized dark (default) and Solarized light, plus the
//! container and text styles the views share.

use iced::theme::Palette;
use iced::widget::{button, container, text};
use iced::{border, color, Background, Border, Color, Shadow, Theme};

/// Solarized base and accent colors (Ethan Schoonover's values).
pub mod sol {
    use iced::{color, Color};

    pub const BASE03: Color = color!(0x002b36);
    pub const BASE02: Color = color!(0x073642);
    pub const BASE01: Color = color!(0x586e75);
    pub const BASE00: Color = color!(0x657b83);
    pub const BASE1: Color = color!(0x93a1a1);
    pub const BASE2: Color = color!(0xeee8d5);
    pub const BASE3: Color = color!(0xfdf6e3);
    pub const YELLOW: Color = color!(0xb58900);
    pub const ORANGE: Color = color!(0xcb4b16);
    pub const RED: Color = color!(0xdc322f);
    pub const MAGENTA: Color = color!(0xd33682);
    pub const VIOLET: Color = color!(0x6c71c4);
    pub const BLUE: Color = color!(0x268bd2);
    pub const CYAN: Color = color!(0x2aa198);
    pub const GREEN: Color = color!(0x859900);
}

pub fn app_theme(light: bool) -> Theme {
    if light {
        Theme::custom(
            "Solarized Light",
            Palette {
                background: sol::BASE3,
                text: sol::BASE00,
                primary: sol::BLUE,
                success: sol::GREEN,
                warning: sol::YELLOW,
                danger: sol::RED,
            },
        )
    } else {
        Theme::custom(
            "Solarized Dark",
            Palette {
                background: sol::BASE03,
                text: color!(0xa7b7b8),
                primary: sol::BLUE,
                success: sol::GREEN,
                warning: sol::YELLOW,
                danger: sol::RED,
            },
        )
    }
}

pub fn is_light(theme: &Theme) -> bool {
    !theme.extended_palette().is_dark
}

/// Panels a step off the background: the sidebar, cards, the composer.
pub fn panel_color(theme: &Theme) -> Color {
    if is_light(theme) {
        sol::BASE2
    } else {
        sol::BASE02
    }
}

/// Code blocks and tool output: a step darker than the page.
pub fn code_color(theme: &Theme) -> Color {
    if is_light(theme) {
        color!(0xf4eedc)
    } else {
        color!(0x00212b)
    }
}

pub fn line_color(theme: &Theme) -> Color {
    if is_light(theme) {
        color!(0xddd6c1)
    } else {
        color!(0x0e4552)
    }
}

/// Body text at reduced opacity, readable on any background.
pub fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(if is_light(theme) {
            sol::BASE1
        } else {
            sol::BASE01.scale_alpha(1.0)
        }),
    }
}

pub fn accent(color: Color) -> impl Fn(&Theme) -> text::Style {
    move |_| text::Style { color: Some(color) }
}

pub fn sidebar(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(panel_color(theme).into()),
        ..container::Style::default()
    }
}

pub fn card(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(panel_color(theme).into()),
        border: Border {
            color: line_color(theme),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn user_bubble(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(panel_color(theme).into()),
        border: Border {
            color: if is_light(theme) {
                sol::BLUE.scale_alpha(0.35)
            } else {
                sol::BLUE.scale_alpha(0.45)
            },
            width: 1.0,
            radius: 14.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn code_box(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(code_color(theme).into()),
        border: Border {
            color: line_color(theme),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn error_box(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(sol::RED.scale_alpha(0.12).into()),
        text_color: Some(if is_light(theme) {
            color!(0xa52a24)
        } else {
            color!(0xf0a09e)
        }),
        border: Border {
            color: sol::RED.scale_alpha(0.6),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

pub fn badge(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(color.scale_alpha(0.18).into()),
        text_color: Some(color),
        border: border::rounded(10),
        ..container::Style::default()
    }
}

/// A tooltip: a raised bubble so the hint never blends into what is under it.
pub fn tooltip_bubble(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(code_color(theme).into()),
        text_color: Some(if is_light(theme) {
            sol::BASE00
        } else {
            sol::BASE1
        }),
        border: Border {
            color: sol::CYAN.scale_alpha(0.5),
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: Shadow {
            color: Color::BLACK.scale_alpha(0.45),
            offset: iced::Vector::new(0.0, 3.0),
            blur_radius: 10.0,
        },
        ..container::Style::default()
    }
}

/// A key in a shortcut: a dark keycap with a cyan rim and a soft glow.
pub fn keycap(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(code_color(theme).into()),
        text_color: Some(sol::CYAN),
        border: Border {
            color: sol::CYAN.scale_alpha(0.55),
            width: 1.0,
            radius: 7.0.into(),
        },
        shadow: Shadow {
            color: sol::CYAN.scale_alpha(if is_light(theme) { 0.15 } else { 0.25 }),
            offset: iced::Vector::new(0.0, 1.5),
            blur_radius: 6.0,
        },
        ..container::Style::default()
    }
}

/// A flat button that lights up on hover; `selected` keeps it lit.
pub fn row_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let base = if is_light(theme) {
            color!(0xe4dcc4)
        } else {
            color!(0x0b4351)
        };
        let background = match (selected, status) {
            (true, _) => Some(Background::from(base)),
            (false, button::Status::Hovered | button::Status::Pressed) => {
                Some(Background::from(base.scale_alpha(0.6)))
            }
            _ => None,
        };
        button::Style {
            background,
            text_color: theme.palette().text,
            border: border::rounded(8),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A small text-like button for icons and secondary actions.
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| Background::from(line_color(theme))),
        text_color: if matches!(status, button::Status::Disabled) {
            theme.palette().text.scale_alpha(0.4)
        } else {
            theme.palette().text
        },
        border: border::rounded(6),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A tab or toggle in a segmented row.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = ghost(theme, status);
        if active {
            style.background = Some(Background::from(sol::BLUE.scale_alpha(0.22)));
            style.text_color = if is_light(theme) {
                color!(0x1a6aa6)
            } else {
                color!(0x6cb6eb)
            };
        }
        style
    }
}
