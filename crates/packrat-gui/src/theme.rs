//! The application's visual language.
//!
//! Iced derives most widget appearance from the theme palette, so this module
//! defines one custom theme plus a small set of surfaces on top of it — cards,
//! chips, table rows, badges and button variants. Every screen builds from
//! these so the window stays visually consistent.

use iced::border::Radius;
use iced::theme::Palette;
use iced::widget::scrollable::{self, AutoScroll, Rail, Scroller};
use iced::widget::{button, checkbox, container, pick_list, text_input};
use iced::{Border, Color, Shadow, Theme, Vector};

/// Window background.
pub const BG: Color = Color::from_rgb8(0x0e, 0x11, 0x17);
/// Card background.
pub const SURFACE: Color = Color::from_rgb8(0x16, 0x1a, 0x22);
/// The top bar. Darker than anything else, so the chrome reads as a layer of
/// its own: a card colour would make it look like another panel on the page.
pub const BAR: Color = Color::from_rgb8(0x08, 0x0a, 0x0e);
/// Raised background: chips, inputs, alternate table rows.
pub const SURFACE_HI: Color = Color::from_rgb8(0x1e, 0x24, 0x2e);
/// Hairline borders and dividers.
pub const BORDER: Color = Color::from_rgb8(0x2a, 0x31, 0x3d);
/// Primary text.
pub const TEXT: Color = Color::from_rgb8(0xe6, 0xea, 0xf2);
/// Secondary text: captions and labels.
pub const MUTED: Color = Color::from_rgb8(0x8b, 0x95, 0xa7);
/// Accent: primary actions and the active state.
pub const ACCENT: Color = Color::from_rgb8(0x3b, 0x82, 0xf6);
pub const SUCCESS: Color = Color::from_rgb8(0x3e, 0xc7, 0x8b);
pub const WARNING: Color = Color::from_rgb8(0xf5, 0xa5, 0x24);
pub const DANGER: Color = Color::from_rgb8(0xf0, 0x61, 0x6d);

/// The application theme, built from the palette above.
pub fn theme() -> Theme {
    Theme::custom(
        "packrat",
        Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            warning: WARNING,
            danger: DANGER,
        },
    )
}

fn radius(px: f32) -> Radius {
    Radius::from(px)
}

fn no_shadow() -> Shadow {
    Shadow {
        color: Color::TRANSPARENT,
        offset: Vector::new(0.0, 0.0),
        blur_radius: 0.0,
    }
}

/// A soft drop shadow, for surfaces that should read as raised.
fn raised() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.40),
        offset: Vector::new(0.0, 3.0),
        blur_radius: 14.0,
    }
}

/// The top bar: the window's chrome rather than part of the page.
pub fn bar(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: Some(BAR.into()),
        border: Border::default(),
        shadow: no_shadow(),
        snap: true,
    }
}

/// The window background.
pub fn page(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: Some(BG.into()),
        border: Border::default(),
        shadow: no_shadow(),
        snap: true,
    }
}

/// A raised panel: the main layout unit.
pub fn card(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: Some(SURFACE.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: radius(14.0),
        },
        shadow: raised(),
        snap: true,
    }
}

/// A table row, flush with the card behind it.
pub fn row(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: None,
        border: Border::default(),
        shadow: no_shadow(),
        snap: true,
    }
}

/// An alternate table row, tinted so the columns stay readable.
pub fn row_alt(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: Some(SURFACE_HI.into()),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: radius(8.0),
        },
        shadow: no_shadow(),
        snap: true,
    }
}

/// A pill badge tinted with `color`, for disc kind and status.
pub fn badge(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme: &Theme| container::Style {
        text_color: Some(color),
        background: Some(Color { a: 0.14, ..color }.into()),
        border: Border {
            color: Color { a: 0.35, ..color },
            width: 1.0,
            radius: radius(999.0),
        },
        shadow: no_shadow(),
        snap: true,
    }
}

/// An inline notice, used for warnings such as a missing libdvdcss.
pub fn notice(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme: &Theme| container::Style {
        text_color: Some(color),
        background: Some(Color { a: 0.10, ..color }.into()),
        border: Border {
            color: Color { a: 0.28, ..color },
            width: 1.0,
            radius: radius(10.0),
        },
        shadow: no_shadow(),
        snap: true,
    }
}

/// The main call to action.
pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::primary(theme, status);
    style.border.radius = radius(9.0);
    style
}

/// A quieter action beside the primary one.
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::secondary(theme, status);
    style.border.radius = radius(9.0);
    style
}

/// A destructive or stopping action.
pub fn danger(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::danger(theme, status);
    style.border.radius = radius(9.0);
    style
}

/// Text fields.
///
/// A disabled field keeps the shape and loses the contrast, so it reads as the
/// same field being unavailable rather than as a different kind of thing.
pub fn input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    let disabled = matches!(status, text_input::Status::Disabled);
    text_input::Style {
        background: (if disabled { SURFACE } else { SURFACE_HI }).into(),
        border: Border {
            color: if focused { ACCENT } else { BORDER },
            width: 1.0,
            radius: radius(9.0),
        },
        icon: if disabled { BORDER } else { MUTED },
        placeholder: MUTED,
        value: if disabled { MUTED } else { TEXT },
        selection: ACCENT,
    }
}

/// A field that cannot be edited, drawn in place of one that can.
///
/// A pick list has no disabled state — it opens whenever it is pressed — so the
/// runs where the disc is fixed draw this instead, at the same size and with the
/// same border, and the message it would have sent is refused anyway.
pub fn field_disabled(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(MUTED),
        background: Some(SURFACE.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: radius(9.0),
        },
        shadow: no_shadow(),
        snap: true,
    }
}

/// Checkboxes, so an empty one still reads as a box rather than a dark hole.
pub fn check(_theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let (checked, hovered, disabled) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };

    // A disabled box keeps its state and loses the accent, so a selection being
    // worked from is visible but plainly not up for changing.
    let (background, border) = if disabled {
        (SURFACE_HI, BORDER)
    } else if checked {
        (ACCENT, ACCENT)
    } else if hovered {
        (SURFACE_HI, ACCENT)
    } else {
        (SURFACE_HI, BORDER)
    };

    checkbox::Style {
        background: background.into(),
        icon_color: if disabled { MUTED } else { Color::WHITE },
        border: Border {
            color: border,
            width: 1.0,
            radius: radius(5.0),
        },
        text_color: Some(if disabled { MUTED } else { TEXT }),
    }
}

/// The page scrollbar, thin and unobtrusive on the dark background.
pub fn scrollbar(_theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let highlighted = matches!(status, scrollable::Status::Hovered { .. });

    let rail = Rail {
        background: None,
        border: Border::default(),
        scroller: Scroller {
            background: if highlighted { ACCENT } else { BORDER }.into(),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: radius(999.0),
            },
        },
    };

    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: AutoScroll {
            background: SURFACE.into(),
            border: Border {
                color: BORDER,
                width: 1.0,
                radius: radius(999.0),
            },
            shadow: no_shadow(),
            icon: MUTED,
        },
    }
}

/// The disc list.
pub fn dropdown(_theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let highlighted = matches!(
        status,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. }
    );

    pick_list::Style {
        text_color: TEXT,
        placeholder_color: MUTED,
        handle_color: MUTED,
        background: SURFACE_HI.into(),
        border: Border {
            color: if highlighted { ACCENT } else { BORDER },
            width: 1.0,
            radius: radius(9.0),
        },
    }
}

/// A row of a list of things to choose between.
///
/// Flat, taking the accent only under the pointer, so a column of them reads as
/// a list rather than as a stack of buttons.
pub fn choice(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::text(theme, status);
    style.border = Border {
        color: BORDER,
        width: 1.0,
        radius: radius(9.0),
    };
    if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        style.background = Some(SURFACE_HI.into());
        style.border.color = ACCENT;
    }
    style
}
