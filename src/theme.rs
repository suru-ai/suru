//! Semantic terminal styles used by built-in renderers.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Theme {
    pub(crate) text: TextRoles,
    pub(crate) surface: SurfaceRoles,
    pub(crate) accent: AccentRoles,
    pub(crate) ansi: AnsiPalette,
    #[allow(dead_code)] // Reserved by the required semantic contract for command affordances.
    pub(crate) action: ActionRoles,
    pub(crate) form_field: FormFieldRoles,
    pub(crate) feedback: FeedbackRoles,
    pub(crate) border: BorderRoles,
    pub(crate) markdown: MarkdownRoles,
    #[allow(dead_code)] // Reserved by the required semantic contract for selectable UI.
    pub(crate) selection: SelectionRoles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRoles {
    pub(crate) primary: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SurfaceRoles {
    pub(crate) elevated: Style,
    pub(crate) overlay: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccentRoles {
    pub(crate) primary: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiPalette {
    pub(crate) normal: AnsiColors,
    pub(crate) bright: AnsiColors,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiColors {
    pub(crate) black: Color,
    pub(crate) red: Color,
    pub(crate) green: Color,
    pub(crate) yellow: Color,
    pub(crate) blue: Color,
    pub(crate) magenta: Color,
    pub(crate) cyan: Color,
    pub(crate) white: Color,
}

impl AnsiPalette {
    pub(crate) fn color(&self, index: u16, bright: bool) -> Option<Color> {
        let colors = if bright { self.bright } else { self.normal };
        Some(match index {
            0 => colors.black,
            1 => colors.red,
            2 => colors.green,
            3 => colors.yellow,
            4 => colors.blue,
            5 => colors.magenta,
            6 => colors.cyan,
            7 => colors.white,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // The roles are defined before command affordances consume them.
pub(crate) struct ActionRoles {
    pub(crate) primary: Style,
    pub(crate) disabled: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FormFieldRoles {
    pub(crate) text: Style,
    pub(crate) placeholder: Style,
    pub(crate) border: Style,
    pub(crate) invalid: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackRoles {
    pub(crate) error: Style,
    pub(crate) warning: Style,
    pub(crate) success: Style,
    #[allow(dead_code)] // Informational feedback has no current transcript variant.
    pub(crate) info: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BorderRoles {
    pub(crate) default: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkdownRoles {
    pub(crate) heading: Style,
    pub(crate) emphasis: Style,
    pub(crate) strong: Style,
    pub(crate) link: Style,
    pub(crate) inline_code: Style,
    pub(crate) code_block: Style,
    pub(crate) list_marker: Style,
}

/// How a surface says which of its rows a reader means. The two questions are
/// separate wherever a list stands beside the thing it lists: which row the
/// keys would act on, and which row is the one already on show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectionRoles {
    /// The row the keys are on, drawn only while the surface holding it has
    /// them.
    pub(crate) focused: Style,
    /// The row a surface that has given the keys up would come back to, where
    /// that surface keeps one. The Sidebar keeps none: its row focus goes with
    /// the keys.
    pub(crate) unfocused: Style,
    /// The row standing for what the main view is already showing, which is
    /// true whoever holds the keys.
    pub(crate) open: Style,
    /// The column down the left of that row, which is the combined state's own
    /// role: it is what says "open" while the row itself carries the focus
    /// style, and so it is the one a theme has to keep legible against
    /// [`Self::focused`] rather than against the surface behind it.
    pub(crate) open_rail: Style,
}

impl Theme {
    pub(crate) fn system() -> Self {
        let accent = AccentRoles {
            primary: Style::default().fg(Color::Cyan),
        };
        // The accent's own colour, carried into a block rather than into text:
        // the open row is the one the accent has always stood for, and a theme
        // that moves the accent moves it. The rail is the same block one
        // column further left, because it stands for the same thing.
        let open = Style::default()
            .fg(Color::Black)
            .bg(accent.primary.fg.unwrap_or(Color::Cyan));
        let feedback = FeedbackRoles {
            error: Style::default().fg(Color::Red),
            warning: Style::default().fg(Color::Yellow),
            success: Style::default().fg(Color::Green),
            info: Style::default().fg(Color::Blue),
        };
        Self {
            text: TextRoles {
                primary: Style::default().fg(Color::Reset),
                subdued: Style::default().fg(Color::DarkGray),
            },
            surface: SurfaceRoles {
                elevated: Style::default().bg(Color::Black),
                overlay: Style::default().bg(Color::Black),
            },
            accent,
            ansi: AnsiPalette {
                normal: AnsiColors {
                    black: Color::Black,
                    red: feedback.error.fg.unwrap_or(Color::Red),
                    green: feedback.success.fg.unwrap_or(Color::Green),
                    yellow: feedback.warning.fg.unwrap_or(Color::Yellow),
                    blue: feedback.info.fg.unwrap_or(Color::Blue),
                    magenta: Color::Magenta,
                    cyan: accent.primary.fg.unwrap_or(Color::Cyan),
                    white: Color::Gray,
                },
                bright: AnsiColors {
                    black: Color::DarkGray,
                    red: Color::LightRed,
                    green: Color::LightGreen,
                    yellow: Color::LightYellow,
                    blue: Color::LightBlue,
                    magenta: Color::LightMagenta,
                    cyan: Color::LightCyan,
                    white: Color::White,
                },
            },
            action: ActionRoles {
                primary: Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                disabled: Style::default().fg(Color::DarkGray),
            },
            form_field: FormFieldRoles {
                text: Style::default().fg(Color::Reset),
                placeholder: Style::default().fg(Color::DarkGray),
                border: Style::default().fg(Color::Cyan),
                invalid: Style::default().fg(Color::Red),
            },
            feedback,
            border: BorderRoles {
                default: Style::default().fg(Color::Gray),
                subdued: Style::default().fg(Color::DarkGray),
            },
            markdown: MarkdownRoles {
                heading: Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                emphasis: Style::default().add_modifier(Modifier::ITALIC),
                strong: Style::default().add_modifier(Modifier::BOLD),
                link: Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
                inline_code: Style::default().fg(Color::Yellow),
                code_block: Style::default().fg(Color::Green),
                list_marker: Style::default().fg(Color::Cyan),
            },
            selection: SelectionRoles {
                focused: Style::default().fg(Color::Black).bg(Color::Blue),
                unfocused: Style::default().fg(Color::Reset).bg(Color::DarkGray),
                open,
                open_rail: open,
            },
        }
    }
}
