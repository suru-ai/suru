//! A Context Breakdown read on request from Copilot's `session.metadata.getContextAttribution`,
//! through the runtime, server and client contract.

use std::time::Duration;

use crate::support::{
    ScriptedCopilot, conversation_arms, opened_session_on, send_arm, settled_session,
};
use suru::{
    protocol::{
        ContextBreakdown, ContextFill, ContextPart, ContextSource, SessionError, SessionErrorCode,
    },
    provider::CopilotRuntime,
};

const TURN: &str = r#"      event answer assistant.message '{"messageId":"m1","content":"Measured"}'
      event idle session.idle '{}'
"#;

/// An attribution shaped as the CLI answered on 2026-10-02: its limits are the runtime default
/// whatever the Model, so free space and buffer are measured against nothing real.
const ATTRIBUTION: &str = r#"{"contextAttribution":{"totalTokens":20500,"modelId":"claude-fixture","modelSource":"selected","promptTokenLimit":128000,"limit":128000,"bufferTokens":30000,"compactionThreshold":100000,"categories":{"systemPrompt":6000,"customInstructions":1500,"systemTools":9000,"mcpTools":0,"messages":4000,"freeSpace":77500,"buffer":30000},"entries":[{"kind":"system","id":"system:prompt","label":"System","tokens":6000}],"compactions":{"count":0}}}"#;

fn attribution_arm(result: &str) -> String {
    format!(
        r#"    *'"method":"session.metadata.getContextAttribution"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{result}}}'
      ;;
"#
    )
}

fn fixture(arm: &str) -> ScriptedCopilot {
    ScriptedCopilot::new(&format!("{}{}{arm}", conversation_arms(), send_arm(TURN)))
}

fn part(source: ContextSource, tokens: u64) -> ContextPart {
    ContextPart {
        source,
        tokens,
        items: Vec::new(),
    }
}

fn refusal(error: &anyhow::Error) -> Option<SessionErrorCode> {
    error.downcast_ref::<SessionError>().map(|error| error.code)
}

#[tokio::test]
async fn a_running_session_breaks_its_context_down_by_category() {
    let copilot = fixture(&attribution_arm(ATTRIBUTION));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable()),
        "copilot-context-breakdown",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let breakdown = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .unwrap();

    assert_eq!(
        breakdown,
        ContextBreakdown {
            fill: ContextFill {
                occupied_tokens: 20500,
                capacity_tokens: None,
            },
            reserved_tokens: None,
            parts: vec![
                part(ContextSource::SystemPrompt, 6000),
                part(ContextSource::Instructions, 1500),
                part(ContextSource::SystemTools, 9000),
                part(ContextSource::McpTools, 0),
                part(ContextSource::Messages, 4000),
            ],
        }
    );
    opened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_copilot_has_not_measured_fails_with_why() {
    let copilot = fixture(&attribution_arm(r#"{"contextAttribution":null}"#));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable()),
        "copilot-context-breakdown-unmeasured",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .expect_err("nothing measured is no breakdown");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownFailed),
        "{refused:#}"
    );
    assert!(
        format!("{refused:#}").contains("not measured"),
        "{refused:#}"
    );
    opened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cli_that_never_answers_fails_once_the_request_times_out() {
    let copilot = fixture("");
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(50)),
        "copilot-context-breakdown-silent",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .expect_err("silence is no breakdown");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownFailed),
        "{refused:#}"
    );
    assert!(format!("{refused:#}").contains("timed out"), "{refused:#}");
    opened.server.shutdown().await.unwrap();
}
