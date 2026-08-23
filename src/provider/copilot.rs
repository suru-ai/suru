//! GitHub Copilot Provider runtime over the Copilot CLI's stdio server mode.
//!
//! The integration is layered like the Codex one, but reaches its harness through the first-party
//! `github-copilot-sdk` crate rather than a hand-rolled wire: the provider-neutral harness
//! machinery owns the CLI process and [`transport`] wraps its stdio in the SDK's client, [`catalog`]
//! normalizes the Models that client reports into Suru's Model catalog, and [`runtime`] composes
//! them into the Provider runtime the rest of Suru uses.
//!
//! Unlike Codex, one Copilot CLI process hosts every Copilot Session, so the runtime owns a single
//! [`shared harness`](super::harness::SharedHarness) that Model discovery and Sessions alike demand.

mod catalog;
mod runtime;
mod transport;

pub use runtime::CopilotRuntime;

use super::{ProviderError, concise_remote_message};

/// What the Log and failures call the Copilot harness server process.
const COPILOT_HARNESS_NAME: &str = "Copilot CLI server";

/// The arguments that put the Copilot CLI in the stdio server mode the SDK speaks to. Suru launches
/// the process itself rather than letting the SDK spawn it, so that the shared-harness machinery
/// owns the process tree; these mirror what the SDK's own stdio transport passes.
const COPILOT_SERVER_ARGS: [&str; 3] = ["--server", "--stdio", "--no-auto-update"];

const REASONING_EFFORT_OPTION_ID: &str = "reasoning_effort";
const CONTEXT_TIER_OPTION_ID: &str = "context_tier";
const DEFAULT_CONTEXT_TIER_CHOICE_ID: &str = "default";

fn copilot_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        "Copilot Provider failed",
    ))
}

/// Wraps `error` in the operation that failed, preserving the classification it already carries.
fn copilot_error_context(context: &str, error: ProviderError) -> ProviderError {
    let session_lost = error.is_session_lost();
    let selection_rejected = error.is_selection_rejected();
    let mut contextual = copilot_error(format!("{context}: {error}"));
    if session_lost {
        contextual = contextual.mark_session_lost();
    }
    if selection_rejected {
        contextual = contextual.mark_selection_rejected();
    }
    contextual
}
