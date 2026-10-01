//! Claude Provider runtime over the Claude Code CLI's stream-json protocol.
//!
//! Suru drives the user's installed Claude Code CLI directly over its stream-json wire — newline-
//! delimited JSON on stdio, with control requests for everything that is not conversation — rather
//! than embedding the Claude Agent SDK (ADR 0010). The integration is layered like the Codex one:
//! [`wire`] holds the serde types Suru exchanges with the CLI, [`transport`] carries control
//! requests and conversation over a supervised CLI process's stdio, [`projection`] turns the
//! conversation into Provider events, [`catalog`] presents the Models the CLI reports verbatim as
//! Suru's Model catalog, [`availability`] decides whether the CLI can be driven at all before
//! anything asks it to work, [`session`] runs one Session per long-lived CLI process,
//! [`turn_in_flight`] holds what a Session and its projection must agree on about the Turn in
//! flight, [`broker`] writes the MCP config each Session launch is handed the Broker through,
//! [`errand`] runs Suru's own one-shot work through the CLI's print mode, and [`runtime`]
//! composes them into the Provider runtime the rest of Suru uses. What the layers share sits here:
//! how a Claude failure reads, and the launch flags an Agent Selection lowers onto, which a
//! Session's child and an Errand's one-shot carry alike.
//!
//! The Provider is named **Claude**, never "Claude Code": the Agent SDK asks applications not to
//! take the product's name. Text below names the Claude Code CLI only where it factually refers to
//! the binary being driven.
//!
//! A CLI that is not installed or has no signed-in user reaches Model discovery — and so a Turn's
//! Session startup — as its own typed reason rather than as a failure. [`availability`] also asks
//! the CLI for its version: one below the verified 2.1.280 suggestion remains usable with advisory
//! guidance, while a CLI too old to answer the probe is incompatible. A catalog refresh re-runs
//! conditions that need the user to fix something outside Suru.

mod approval;
mod availability;
mod broker;
mod catalog;
mod compaction;
mod context;
mod errand;
mod projection;
mod questionnaire;
mod runtime;
mod session;
mod skills;
mod thinking;
mod transport;
mod turn_in_flight;
mod wire;

pub use runtime::ClaudeRuntime;

use std::ffi::OsString;

use crate::protocol::{AgentSelection, ModelOptionValue};

use super::{ProviderError, concise_remote_message};

/// What the Log and failures call the Claude harness process.
const CLAUDE_HARNESS_NAME: &str = "Claude Code CLI";

/// What they call the one-shot Claude run an Errand is, which is the same binary in a different
/// mode from the stream-json process above — and, unlike it, one Suru never speaks a protocol to.
const CLAUDE_ONE_SHOT_NAME: &str = "Claude Code CLI print mode";

/// What Suru knows this Provider as.
const CLAUDE_PROVIDER_ID: &str = "claude";

/// What the Transcript attributes this Provider's Turns to.
const CLAUDE_AGENT_ID: &str = "claude";

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

/// The user-readable account of a result the CLI ended a loop with badly, told as a failure of
/// `what` — a Turn, an Errand.
///
/// The first user-facing error the CLI reported comes first — `[ede_diagnostic]` entries are
/// CLI-internal telemetry the CLI hides from its own UI — then the result text an errored `success`
/// carries, then the bare subtype when the CLI said nothing more.
fn result_failure_message(what: &str, result: &wire::ResultMessage) -> String {
    let reported = result
        .errors
        .iter()
        .find(|error| !error.starts_with("[ede_diagnostic]"))
        .map(String::as_str)
        .or_else(|| result.result.as_ref().and_then(serde_json::Value::as_str))
        .filter(|reported| !reported.trim().is_empty());
    match reported {
        Some(reported) => concise_remote_message(
            &format!("Claude {what} failed: {reported}"),
            CLAUDE_FAILURE_FALLBACK,
        ),
        None => format!(
            "Claude {what} failed: the Claude Code CLI reported `{}`",
            result.subtype
        ),
    }
}

/// The launch flags an Agent Selection lowers onto. The Model and its reasoning effort are
/// spawn-time flags on the CLI rather than anything it takes over its wire, so every process Suru
/// launches to do work — a Session's child and an Errand's one-shot alike — carries the Selection
/// this way.
fn selection_args(selection: &AgentSelection) -> Result<Vec<OsString>, ProviderError> {
    debug_assert_eq!(selection.provider.as_str(), CLAUDE_PROVIDER_ID);
    let mut args = vec![
        OsString::from("--model"),
        OsString::from(selection.model.as_str()),
    ];
    let mut effort = None;
    for option in &selection.options {
        let ModelOptionValue::Select { choice } = &option.value else {
            return Err(ProviderError::selection_rejected(format!(
                "Claude does not support toggle Model Option `{}`",
                option.id
            )));
        };
        match option.id.as_str() {
            REASONING_EFFORT_OPTION_ID if effort.is_none() => {
                effort = Some(choice.as_str().to_owned());
            }
            REASONING_EFFORT_OPTION_ID => {
                return Err(ProviderError::selection_rejected(format!(
                    "Claude Model Option `{}` was selected more than once",
                    option.id
                )));
            }
            _ => {
                return Err(ProviderError::selection_rejected(format!(
                    "Claude does not support Model Option `{}`",
                    option.id
                )));
            }
        }
    }
    if let Some(effort) = effort {
        args.push(OsString::from("--effort"));
        args.push(OsString::from(effort));
    }
    Ok(args)
}
