//! Headless rendering and input tests driven through the production TUI renderer.
//!
//! Each module below covers one area of the interface; `support` holds the fixtures
//! and rendering helpers shared between them.

#[path = "../support/failing_provider.rs"]
mod failing_provider_support;

mod commands;
mod composer;
mod landing_notice;
mod model_options;
mod model_picker;
mod prompts;
mod reasoning_cycle;
mod session_picker;
mod shell;
mod support;
mod transcript;
