//! Codex reports how full its context is, but not what fills it.

use crate::support::{conversation_codex, opened_session, settled_session};
use suru::protocol::{SessionError, SessionErrorCode};

const COMPLETED: &str = r#"      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_codex_session_has_no_context_breakdown_to_give() {
    let codex = conversation_codex(COMPLETED);
    let opened = opened_session(&codex, "codex-context-breakdown", "Get to work").await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .expect_err("Codex attributes none of its context");

    assert_eq!(
        refused
            .downcast_ref::<SessionError>()
            .map(|error| error.code),
        Some(SessionErrorCode::ContextBreakdownUnsupported),
        "{refused:#}"
    );
    opened.server.shutdown().await.expect("shut down server");
}
