//! Claude Provider runtime over the Claude Code CLI's stream-json protocol.
//!
//! Suru drives the user's installed Claude Code CLI directly over its stream-json wire — newline-
//! delimited JSON on stdio, with control requests for everything that is not conversation — rather
//! than embedding the Claude Agent SDK (ADR 0010). The integration is layered like the Codex one:
//! [`wire`] holds the serde types Suru exchanges with the CLI, [`transport`] carries control
//! requests over a supervised CLI process's stdio, [`catalog`] presents the Models the CLI reports
//! verbatim as Suru's Model catalog, and [`runtime`] composes them into the Provider runtime the
//! rest of Suru uses.
//!
//! The Provider is named **Claude**, never "Claude Code": the Agent SDK asks applications not to
//! take the product's name. Text below names the Claude Code CLI only where it factually refers to
//! the binary being driven.

mod catalog;
mod runtime;
mod transport;
mod wire;

pub use runtime::ClaudeRuntime;

use super::{ProviderError, concise_remote_message};

/// What the Log and failures call the Claude harness process.
const CLAUDE_HARNESS_NAME: &str = "Claude Code CLI";

/// What Suru knows this Provider as.
const CLAUDE_PROVIDER_ID: &str = "claude";

const REASONING_EFFORT_OPTION_ID: &str = "reasoning_effort";

/// What a Claude failure is called when the CLI itself said nothing usable.
const CLAUDE_FAILURE_FALLBACK: &str = "Claude Provider failed";

fn claude_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        CLAUDE_FAILURE_FALLBACK,
    ))
}

/// Wraps `error` in the operation that failed, preserving the classification it already carries.
fn claude_error_context(context: &str, error: ProviderError) -> ProviderError {
    let message = concise_remote_message(&format!("{context}: {error}"), CLAUDE_FAILURE_FALLBACK);
    error.reworded(message)
}
