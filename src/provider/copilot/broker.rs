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
//!
//! Beside the server list, each of those requests carries the Broker's note, as the handoff writes
//! it for its Agent, as a system message in append mode, added to Copilot's own rather than
//! replacing any of it, and naming each Tool as Copilot names an MCP server's Tools to its Agent —
//! `suru-spawn_subagent`.

use std::collections::HashMap;

use github_copilot_sdk::{IndexMap, McpHttpServerConfig, McpServerConfig, SystemMessageConfig};

use crate::{
    broker::{BROKER_CALL_TIMEOUT_MS, BROKER_SERVER_NAME},
    provider::BrokerHandoff,
};

/// The server list a Session handed `handoff` is created or resumed with.
pub(super) fn broker_mcp_servers(handoff: &BrokerHandoff) -> IndexMap<String, McpServerConfig> {
    let (header, value) = handoff.authorization_header();
    IndexMap::from_iter([(
        BROKER_SERVER_NAME.to_owned(),
        McpServerConfig::Http(McpHttpServerConfig {
            tools: None,
            timeout: Some(i64::from(BROKER_CALL_TIMEOUT_MS)),
            url: handoff.endpoint().as_str().to_owned(),
            headers: HashMap::from([(header.to_owned(), value)]),
        }),
    )])
}

/// The system message a Session handed `handoff` is created or resumed with: the Broker's note,
/// appended to Copilot's own.
pub(super) fn broker_system_message(handoff: &BrokerHandoff) -> SystemMessageConfig {
    SystemMessageConfig::new()
        .with_mode("append")
        .with_content(handoff.instruction_note(|tool| format!("{BROKER_SERVER_NAME}-{tool}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{broker_mcp_servers, broker_system_message};
    use crate::provider::BrokerHandoff;

    #[test]
    fn the_note_is_appended_naming_the_brokers_tools_as_copilot_does() {
        let handoff = BrokerHandoff::for_tests("http://127.0.0.1:1/broker");
        let message =
            serde_json::to_value(broker_system_message(&handoff)).expect("the message serializes");
        assert_eq!(message["mode"], "append");
        assert!(message.get("sections").is_none(), "{message}");
        assert!(
            message["content"]
                .as_str()
                .is_some_and(|note| note.contains("suru-spawn_subagent")),
            "{message}"
        );
    }

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
