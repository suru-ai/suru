//! Ratatui view state and terminal lifecycle.

mod agent_selection_list;
mod approval;
mod approval_posture_picker;
mod aside;
mod attachment_check;
mod attachment_preview;
mod clipboard;
mod clipboard_paste;
mod commands;
mod completion;
mod composer;
mod connect_overlay;
mod context_overlay;
mod directory_browser;
mod drawn_pane;
mod event_loop;
mod fuzzy;
mod hyperlink;
mod icon_picker;
mod keymap;
mod list_editor;
mod list_window;
mod markdown;
mod model_details;
mod model_options;
mod model_picker;
mod notice;
mod onboarding;
mod provider_icons;
mod questionnaire;
mod relay_overlay;
mod rename_dialog;
mod render;
mod selection;
mod serve_overlay;
mod session_attach;
mod session_listing;
mod session_picker;
mod setting_details;
mod settings_panel;
mod shimmer;
mod side_column;
mod sidebar;
mod slots;
mod spinner;
mod state;
mod subagent_picker;
mod text_binding;
mod text_layout;
mod theme_picker;
mod transcript;
mod unreachable_reason;
mod usage;
mod workspace_picker;
mod worktree_picker;
mod wrapped_text;

pub use crate::terminal::{
    CellSize, GraphicsProtocol, TerminalColor, TerminalColorProbe, TerminalFacts,
};
pub use attachment_check::AttachmentCheckId;
pub use attachment_preview::{Thumbnail, ThumbnailRequest};
pub use clipboard::{ClipboardContent, ClipboardRead, PasteId};
pub use commands::{NumericDigit, SemanticCommandId};
pub use completion::CompletionMode;
pub use connect_overlay::ConnectRequest;
pub use context_overlay::ContextBreakdownRefusal;
pub use directory_browser::DirectoryListingId;
pub use event_loop::run;
pub use keymap::command_for_terminal_event;
pub use relay_overlay::{RelayLoginFollow, RelayRequest};
pub use serve_overlay::ServeRequest;
pub use state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, EverywhereListRequest,
    ModelListRequest, ScrollDirection, SessionListRequest, SessionListScope, SessionListSurface,
    SettingRequest, TerminalEventTaken, TuiState, WorkspaceListRequest, WorkspaceResolutionSurface,
};
