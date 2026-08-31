//! Ratatui view state and terminal lifecycle.

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
mod transcript;
mod usage;
mod workspace_picker;

pub use commands::{NumericDigit, SemanticCommandId};
pub use completion::CompletionMode;
pub use event_loop::run;
pub use keymap::command_for_terminal_event;
pub use render::render;
pub use state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, ModelListRequest,
    SessionListRequest, SessionListScope, SessionListSurface, TuiState, WorkspaceResolutionSurface,
};
