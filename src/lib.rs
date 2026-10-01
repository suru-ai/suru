mod ansi;
pub mod approval;
mod attachments;
mod broker;
pub mod build_identity;
mod clock;
mod errands;
mod icon_catalog;
pub mod logging;
pub mod managed_client;
mod model_catalog;
pub mod paths;
pub mod pricing;
pub mod protocol;
pub mod provider;
mod runtime;
pub mod server;
mod serving;
mod session_projection;
mod sessions;
pub mod settings;
mod sidekick;
mod skill_catalog;
pub mod source_control;
mod storage;
mod terminal;
mod theme;
pub mod tui;

pub use runtime::RuntimeConfig;

pub mod questionnaire;

/// The Icon Catalog's names, in listing order — the same list the Title
/// Errand's reply schema enumerates an Icon over. The Catalog itself stays
/// crate-private; this is a deliberate, narrow seam so integration tests (a
/// separate crate) can assert a schema's `icon` enum matches it without
/// reaching into `icon_catalog` itself.
pub fn icon_catalog_names() -> &'static [&'static str] {
    icon_catalog::names()
}
