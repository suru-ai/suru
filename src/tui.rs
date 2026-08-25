//! Ratatui view state and terminal lifecycle.

mod commands;
mod composer;
mod event_loop;
mod keymap;
mod markdown;
mod model_options;
mod model_picker;
mod notice;
mod render;
mod session_picker;
mod settings_panel;
mod slots;
mod spinner;
mod state;
mod transcript;

pub use commands::{NumericDigit, SemanticCommandId};
pub use event_loop::run;
pub use keymap::command_for_terminal_event;
pub use render::render;
pub use state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, ModelListRequest,
    SessionListRequest, SessionListScope, TuiState,
};
