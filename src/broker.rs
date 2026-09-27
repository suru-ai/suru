//! The Broker: the Tools Suru itself offers every Provider Session, served as
//! a streamable-HTTP MCP endpoint on the Server's loopback listener (ADR 0034).
//!
//! Each time a Session's Provider is started it is handed a [`BrokerHandoff`]
//! — the endpoint, and a bearer token naming that Session — on its start
//! request, beside the Approval Posture it already carries. The token lives in
//! a [`BrokerGrant`] held beside that Provider connection and is retired when
//! the connection closes, so a relaunch is handed a fresh one. Every request
//! to the endpoint resolves its token to a [`BrokerCaller`] before any Tool
//! runs, and a token never minted or already retired is refused.
//!
//! The endpoint is loopback only: Serving never forwards it to a Peer, and
//! nothing about it is written into the runtime descriptor, whose token grants
//! the whole API rather than one Session's Agent. Errands carry no Tools, so
//! they are never handed the Broker. With the `broker.enabled` Setting off no
//! Provider start is handed an endpoint and the endpoint answers nothing.
//!
//! Everything MCP-specific stays in [`mcp`]; the Tools themselves, in Suru's
//! own terms, are in [`tools`].

mod access;
mod mcp;
mod tools;

use tokio::sync::watch;

pub(crate) use access::{BrokerAccess, BrokerCaller, BrokerGrant};
pub use access::{BrokerEndpoint, BrokerHandoff, BrokerToken};
pub(crate) use tools::BrokerTools;

/// Where the Broker is served on the Server's loopback listener.
pub(crate) const BROKER_PATH: &str = "/broker";

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
