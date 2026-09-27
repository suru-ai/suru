//! The Broker as a Copilot Session is handed it (ADR 0034).
//!
//! One Copilot CLI process serves every Copilot Session, so the Broker cannot be handed to a
//! process the way Claude's and Codex's are: the only seam Copilot offers a single Session is the
//! MCP server list on `create_session` and on `resume_session`, and each Session opens a connection
//! of its own to a server named there, presenting the headers its own list gave
//! (docs/validation/0408-copilot-mcp-tool-timeout.md). So each of those requests names the Broker
//! as the HTTP server `suru`, with the token that Session's start was handed as its
//! `Authorization` header and a per-server `timeout` in place of the 180 seconds of silence Copilot
//! otherwise allows a call; every Tool the Broker serves is offered. Suru's list names that one
//! server and nothing else. Whether Copilot merges it with the servers its own configuration
//! discovers, or lets it replace them, no capture has established; the live smoke (#421) is to
//! confirm it.
//!
//! Copilot asks Suru's permission handler before every MCP call, and the handler approves the
//! Broker's own ([`super::approval`]); the projection absorbs the Broker's tool executions, which
//! the Broker's own rows answer for ([`super::projection`]).

use std::collections::HashMap;

use github_copilot_sdk::{IndexMap, McpHttpServerConfig, McpServerConfig};

use crate::{
    broker::{BROKER_CALL_TIMEOUT, BROKER_SERVER_NAME},
    provider::BrokerHandoff,
};

/// The server list a Session handed `handoff` is created or resumed with.
pub(super) fn broker_mcp_servers(handoff: &BrokerHandoff) -> IndexMap<String, McpServerConfig> {
    let (header, value) = handoff.authorization_header();
    let timeout_ms = i64::try_from(BROKER_CALL_TIMEOUT.as_millis())
        .expect("the Broker's call timeout is a whole number of milliseconds an i64 holds");
    IndexMap::from_iter([(
        BROKER_SERVER_NAME.to_owned(),
        McpServerConfig::Http(McpHttpServerConfig {
            tools: None,
            timeout: Some(timeout_ms),
            url: handoff.endpoint().as_str().to_owned(),
            headers: HashMap::from([(header.to_owned(), value)]),
        }),
    )])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::broker_mcp_servers;
    use crate::provider::BrokerHandoff;

    #[test]
    fn the_broker_is_the_one_http_server_with_its_token_and_call_timeout() {
        let handoff = BrokerHandoff::for_tests("http://127.0.0.1:1/broker");
        assert_eq!(
            serde_json::to_value(broker_mcp_servers(&handoff)).expect("the list serializes"),
            json!({
                "suru": {
                    "type": "http",
                    "url": "http://127.0.0.1:1/broker",
                    "headers": {"Authorization": handoff.token().bearer()},
                    "timeout": 900_000,
                },
            })
        );
    }
}
