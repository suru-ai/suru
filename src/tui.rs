//! Ratatui view state and terminal lifecycle.

mod approval;
mod approval_posture_picker;
mod aside;
mod clipboard;
mod commands;
mod completion;
mod composer;
mod connect_overlay;
mod event_loop;
mod fuzzy;
mod hyperlink;
mod icon_picker;
mod keymap;
mod markdown;
mod model_options;
mod model_picker;
mod notice;
mod questionnaire;
mod render;
mod selection;
mod serve_overlay;
mod session_attach;
mod session_listing;
mod session_picker;
mod settings_panel;
mod shimmer;
mod side_column;
mod sidebar;
mod slots;
mod spinner;
mod state;
mod subagent_picker;
mod text_layout;
mod theme_picker;
mod transcript;
mod usage;
mod workspace_picker;
mod worktree_picker;

pub use crate::terminal::{TerminalColor, TerminalColorProbe, TerminalFacts};
pub use clipboard::ClipboardContent;
pub use commands::{NumericDigit, SemanticCommandId};
pub use completion::CompletionMode;
pub use event_loop::run;
pub use keymap::command_for_terminal_event;
pub use state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, EverywhereListRequest,
    ModelListRequest, ScrollDirection, SessionListRequest, SessionListScope, SessionListSurface,
    TuiState, WorkspaceResolutionSurface,
};
