//! Headless rendering and input tests driven through the production TUI renderer.
//!
//! Each module below covers one area of the interface; `support` holds the fixtures
//! and rendering helpers shared between them.

#[path = "../support/deadlines.rs"]
mod deadlines;

#[path = "../support/failing_provider.rs"]
mod failing_provider_support;

mod approval_posture;
mod approvals;
mod aside;
mod aside_neighbours;
mod commands;
mod composer;
mod connecting;
mod content_width;
mod icon_picker;
mod image_paste;
mod interventions;
mod landing_notice;
mod model_options;
mod model_picker;
mod monitoring;
mod prompts;
mod provisional_session;
mod reasoning_cycle;
mod serving;
mod session_header;
mod session_picker;
mod settings_panel;
mod shell;
mod sidebar;
mod subagent_picker;
mod subagent_view;
mod support;
mod theme;
mod transcript;
mod transcript_attachments;
mod workspace_paths;
mod workspace_picker;

mod questionnaires;

mod selection;

mod worktrees;

mod unreachable;
