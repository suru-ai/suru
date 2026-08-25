//! Codex app-server Provider runtime over its stable V2 stdio protocol.
//!
//! The integration is layered: [`wire`] holds every serde type Codex speaks and its conversions to
//! Suru's protocol, the provider-neutral harness process machinery owns the app-server child
//! processes, [`transport`] carries JSON-RPC over one of those processes, [`projection`] turns
//! native notifications into Provider events with the provider-neutral
//! [`reasoning`](super::reasoning) splitter taking the title off a streamed Reasoning summary, and
//! [`session`] composes them into the Provider runtime and Session the rest of Suru uses.

mod errand;
mod projection;
mod session;
mod skills;
mod transport;
mod wire;

pub use session::CodexRuntime;

use super::{ProviderError, concise_remote_message};

/// What the Log and failures call the Codex harness server process.
const CODEX_HARNESS_NAME: &str = "Codex app-server";

/// What the Log and failures call the one-shot Codex run an Errand is, which is
/// a different process in a different mode from the app-server above.
const CODEX_ONE_SHOT_NAME: &str = "Codex exec";

const REASONING_EFFORT_OPTION_ID: &str = "reasoning_effort";
const SERVICE_TIER_OPTION_ID: &str = "service_tier";
const DEFAULT_SERVICE_TIER_CHOICE_ID: &str = "default";

/// What a Codex failure is called when Codex itself said nothing usable.
const CODEX_FAILURE_FALLBACK: &str = "Codex Provider failed";

fn codex_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        CODEX_FAILURE_FALLBACK,
    ))
}

/// Wraps `error` in the operation that failed, preserving the classification it already carries.
fn codex_error_context(context: &str, error: ProviderError) -> ProviderError {
    let message = concise_remote_message(&format!("{context}: {error}"), CODEX_FAILURE_FALLBACK);
    error.reworded(message)
}
