//! Semantic terminal styles used by built-in renderers.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub(crate) text: TextRoles,
    pub(crate) surface: SurfaceRoles,
    pub(crate) accent: AccentRoles,
    #[allow(dead_code)] // Reserved by the required semantic contract for command affordances.
    pub(crate) action: ActionRoles,
    pub(crate) form_field: FormFieldRoles,
    pub(crate) feedback: FeedbackRoles,
    pub(crate) border: BorderRoles,
    pub(crate) markdown: MarkdownRoles,
    #[allow(dead_code)] // Reserved by the required semantic contract for selectable UI.
    pub(crate) selection: SelectionRoles,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TextRoles {
    pub(crate) primary: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceRoles {
    pub(crate) elevated: Style,
    pub(crate) overlay: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AccentRoles {
    pub(crate) primary: Style,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // The roles are defined before command affordances consume them.
pub(crate) struct ActionRoles {
    pub(crate) primary: Style,
    pub(crate) disabled: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FormFieldRoles {
    pub(crate) text: Style,
    pub(crate) placeholder: Style,
    pub(crate) border: Style,
    pub(crate) invalid: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FeedbackRoles {
    pub(crate) error: Style,
    pub(crate) warning: Style,
    pub(crate) success: Style,
    #[allow(dead_code)] // Informational feedback has no current transcript variant.
    pub(crate) info: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BorderRoles {
    #[allow(dead_code)] // Default borders are available for upcoming non-subdued panels.
    pub(crate) default: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MarkdownRoles {
    pub(crate) heading: Style,
    pub(crate) emphasis: Style,
    pub(crate) strong: Style,
    pub(crate) link: Style,
    pub(crate) inline_code: Style,
    pub(crate) code_block: Style,
    pub(crate) list_marker: Style,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Selection UI is outside this issue but the Theme contract includes it.
pub(crate) struct SelectionRoles {
    pub(crate) focused: Style,
    pub(crate) unfocused: Style,
}

impl Theme {
    pub(crate) fn system() -> Self {
        Self {
            text: TextRoles {
                primary: Style::default().fg(Color::Reset),
                subdued: Style::default().fg(Color::DarkGray),
            },
            surface: SurfaceRoles {
                elevated: Style::default().bg(Color::Black),
                overlay: Style::default().bg(Color::Black),
            },
            accent: AccentRoles {
                primary: Style::default().fg(Color::Cyan),
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
            feedback: FeedbackRoles {
                error: Style::default().fg(Color::Red),
                warning: Style::default().fg(Color::Yellow),
                success: Style::default().fg(Color::Green),
                info: Style::default().fg(Color::Blue),
            },
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
            },
        }
    }
}
