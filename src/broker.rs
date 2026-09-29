//! The Broker: the Tools Suru itself offers every Provider Session, served as
//! a streamable-HTTP MCP endpoint on the Server's loopback listener (ADR 0034).
//!
//! Each time a Session's Provider is started it is handed a [`BrokerHandoff`]
//! — the endpoint, and a bearer token naming that Session — on its start
//! request, beside the Approval Posture it already carries. The token lives in
//! a [`BrokerGrant`] held beside that Provider connection and is retired when
//! the connection closes, so a relaunch is handed a fresh one. Every request
//! to the endpoint resolves its token to a [`BrokerCaller`] before any Tool
//! runs, and a token never minted or already retired is refused. A native
//! Subagent shares its parent's Provider connection, and so its token; a call
//! that names the native Subagent making it — as Codex's name the calling
//! thread — is attributed to that Subagent's Session, and every other call to
//! the token's (ADR 0035).
//!
//! The endpoint is loopback only: Serving never forwards it to a Peer, and
//! nothing about it is written into the runtime descriptor, whose token grants
//! the whole API rather than one Session's Agent. Errands carry no Tools, so
//! they are never handed the Broker. With the `broker.enabled` Setting off no
//! Provider start is handed an endpoint and the endpoint answers nothing.
//!
//! Everything MCP-specific stays in [`mcp`]; the Tools themselves, in Suru's
//! own terms, are in [`tools`]. What each harness is handed is lowered by that
//! harness onto its own per-Session seam, from the handoff and the constants
//! here, which are the same in every one — and so is the note each harness
//! appends to its Agent's instructions, in [`instructions`].

mod access;
mod instructions;
mod mcp;
mod tools;
mod wait;

use std::time::Duration;

use tokio::sync::watch;

pub(crate) use access::{BrokerAccess, BrokerCaller, BrokerGrant};
pub use access::{BrokerEndpoint, BrokerHandoff, BrokerToken};
pub(crate) use instructions::instruction_note;
pub(crate) use tools::BrokerTools;
pub(crate) use wait::WaitTimings;

/// Where the Broker is served on the Server's loopback listener.
pub(crate) const BROKER_PATH: &str = "/broker";

/// The name every harness knows the Broker by among its MCP servers. It is
/// what an Agent's Broker Tools go by — `mcp__suru__list_providers` to
/// Claude — and what Suru recognizes the Broker's calls by where a harness asks
/// Suru to permit them.
pub(crate) const BROKER_SERVER_NAME: &str = "suru";

/// Whether a call of the Broker Tool `name` — named as the Broker names it,
/// without the prefix a harness adds — is recorded by the Subagent row it
/// affects, and so is no Tool Call on any Provider.
pub(crate) fn tool_affects_a_subagent_row(name: &str) -> bool {
    tools::BrokerTool::named(name).is_some_and(tools::BrokerTool::affects_a_subagent_row)
}

/// How long a harness lets one Broker call run, in the milliseconds Claude's
/// and Copilot's per-server `timeout` take. Each harness takes its per-server
/// timeout as a hard limit on the whole call — progress extends neither
/// Claude's nor Codex's, and Copilot's replaces the 180 seconds of silence it
/// otherwise allows — so this stands above the Broker's longest call, a wait
/// at its 600-second ceiling (`docs/validation/0408-*`). A `u32`, so every
/// harness's integer type holds it without a fallible conversion.
pub(crate) const BROKER_CALL_TIMEOUT_MS: u32 = 900_000;

/// [`BROKER_CALL_TIMEOUT_MS`] as a duration, for a harness that takes the
/// timeout in other units — Codex's `tool_timeout_sec`.
pub(crate) const BROKER_CALL_TIMEOUT: Duration =
    Duration::from_millis(BROKER_CALL_TIMEOUT_MS as u64);

/// Whether `path` addresses the Broker, which is never forwarded to a Peer.
pub(crate) fn is_broker_path(path: &str) -> bool {
    path.strip_prefix(BROKER_PATH)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The Broker's routes, for the loopback router to merge. They stop answering
/// long calls once `shutdown` is signalled.
pub(crate) fn router(
    access: BrokerAccess,
    tools: BrokerTools,
    shutdown: watch::Receiver<bool>,
) -> axum::Router {
    mcp::router(access, tools, shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_broker_and_what_lies_beneath_it_are_broker_paths() {
        for path in ["/broker", "/broker/", "/broker/mcp"] {
            assert!(is_broker_path(path), "{path}");
        }
        for path in ["/", "/brokers", "/v1/broker", "/v1/sessions"] {
            assert!(!is_broker_path(path), "{path}");
        }
    }
}
