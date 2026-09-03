//! Ratatui view state and terminal lifecycle.

mod attachment;
mod commands;
mod completion;
mod composer;
mod connect_overlay;
mod event_loop;
mod fuzzy;
mod keymap;
mod markdown;
mod model_options;
mod model_picker;
mod notice;
mod render;
mod serve_overlay;
mod session_listing;
mod session_picker;
mod settings_panel;
mod shimmer;
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

pub use crate::terminal::{TerminalColor, TerminalColorProbe, TerminalFacts};
pub use commands::{NumericDigit, SemanticCommandId};
pub use completion::CompletionMode;
pub use event_loop::run;
pub use keymap::command_for_terminal_event;
pub use state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, EverywhereListRequest,
    ModelListRequest, SessionListRequest, SessionListScope, SessionListSurface, TuiState,
    WorkspaceResolutionSurface,
};
