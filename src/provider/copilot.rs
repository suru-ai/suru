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
//! [`session`] opens one Copilot Session on that process and runs its Turns, [`projection`] turns
//! the Session's event timeline into the Provider events the rest of Suru consumes, and [`tools`]
//! decides what one Tool execution reads as once it gets there. [`errand`] runs Suru's own
//! Errands on the same process, through a Copilot Session it opens and discards, because Copilot's
//! harness offers no one-shot mode to run them without one.
//!
//! Three conditions leave Copilot unusable until the user fixes them outside Suru, and each reaches
//! a Model discovery as its own typed reason rather than as a failure: a CLI that is not installed,
//! which the harness machinery reports for every Provider alike; one speaking another protocol than
//! the SDK, which the handshake in [`transport`] refuses and which a CLI old enough to predate the
//! requests that follow gives itself away in anyway; and one holding no credentials, which the same
//! module asks the CLI about before every discovery. Suru handles no credentials itself, so the
//! catalog refresh re-running all three checks is the whole of its part in the recovery.

mod approval;
mod catalog;
mod errand;
mod event_drain;
mod pricing;
mod projection;
mod questionnaire;
mod runtime;
mod session;
mod skills;
mod tools;
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

/// What Suru knows this Provider as, and the Agent its Sessions' Turns are attributed to.
const COPILOT_PROVIDER_ID: &str = "copilot";
const COPILOT_AGENT_ID: &str = "copilot";

/// What Copilot records as the application driving its Sessions.
const COPILOT_CLIENT_NAME: &str = "suru";

/// What a Copilot failure is called when Copilot itself said nothing usable.
const COPILOT_FAILURE_FALLBACK: &str = "Copilot Provider failed";

fn copilot_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        COPILOT_FAILURE_FALLBACK,
    ))
}

/// Wraps `error` in the operation that failed, preserving the classification it already carries.
fn copilot_error_context(context: &str, error: ProviderError) -> ProviderError {
    let message = concise_remote_message(&format!("{context}: {error}"), COPILOT_FAILURE_FALLBACK);
    error.reworded(message)
}
