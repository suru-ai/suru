//! Codex app-server Provider runtime over its stable V2 stdio protocol.
//!
//! The integration is layered: [`wire`] holds every serde type Codex speaks and its conversions to
//! Suru's protocol, [`process`] owns the app-server child processes, [`transport`] carries JSON-RPC
//! over one of those processes, [`projection`] turns native notifications into Provider events with
//! [`reasoning`] splitting the title off a streamed Reasoning summary, and [`session`] composes them
//! into the Provider runtime and Session the rest of Suru uses.

mod process;
mod projection;
mod reasoning;
mod session;
mod shell_wrapper;
mod transport;
mod wire;

pub use session::CodexRuntime;

use super::ProviderError;

/// Codex messages are user-visible once they surface as a Provider failure, so they are capped.
const MAX_REMOTE_ERROR_CHARS: usize = 384;

/// The Reasoning summary detail Suru asks Codex for on every Turn.
///
/// Codex resolves a Turn's summary detail as the client's setting or, failing
/// that, the Model's own default — and every Model in the current catalog ships
/// that default as `none`. A client that never states a preference therefore
/// gets Reasoning with no summary at all: nothing streams and the completed
/// block carries an empty summary. Suru asks for `auto` and lets the Model
/// choose how much to say. Recorded as a candidate setting in issue #71.
const REASONING_SUMMARY_DETAIL: &str = "auto";

const REASONING_EFFORT_OPTION_ID: &str = "reasoning_effort";
const SERVICE_TIER_OPTION_ID: &str = "service_tier";
const DEFAULT_SERVICE_TIER_CHOICE_ID: &str = "default";

/// Collapses a Codex-authored message onto a single bounded line fit for a Provider failure.
fn concise_remote_message(message: &str, fallback: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let message = if single_line.is_empty() {
        fallback
    } else {
        &single_line
    };
    let mut chars = message.chars();
    let mut concise = chars
        .by_ref()
        .take(MAX_REMOTE_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        concise.push('…');
    }
    concise
}

fn codex_error(message: impl AsRef<str>) -> ProviderError {
    ProviderError::new(concise_remote_message(
        message.as_ref(),
        "Codex Provider failed",
    ))
}

/// Wraps `error` in the operation that failed, preserving the classification it already carries.
fn codex_error_context(context: &str, error: ProviderError) -> ProviderError {
    let session_lost = error.is_session_lost();
    let selection_rejected = error.is_selection_rejected();
    let mut contextual = codex_error(format!("{context}: {error}"));
    if session_lost {
        contextual = contextual.mark_session_lost();
    }
    if selection_rejected {
        contextual = contextual.mark_selection_rejected();
    }
    contextual
}
