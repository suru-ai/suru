//! What Copilot does while it works, in the Transcript: the Reasoning it reports as it reaches an
//! answer, and the Tool executions it reports as Command Activity.

use std::sync::Arc;

use crate::support::{agent_messages, connect, conversation_fixture, settled_session};
use suru::{
    protocol::{
        Activity, ActivityStatus, CreateSessionRequest, InitialPrompt, PromptId, TranscriptItem,
        TurnStatus,
    },
    provider::CopilotRuntime,
    server::{self, ServerConfig},
};

/// A Turn that reasons, runs a command, reads a file, and answers. The shell Tool is reported the
/// way Copilot's shell driver reports one it rewrote: the arguments the Model wrote, and beside
/// them the command that actually ran.
const WORKING_TURN: &str = r#"      event e1 assistant.reasoning_delta '{"reasoningId":"r1","deltaContent":"**Reading the workspace**\n\nListing"}'
      event e2 assistant.reasoning_delta '{"reasoningId":"r1","deltaContent":" what is there."}'
      event e3 assistant.reasoning '{"reasoningId":"r1","content":"**Reading the workspace**\n\nListing what is there."}'
      event e4 tool.execution_start '{"toolCallId":"t1","toolName":"bash","arguments":{"command":"cd /workspace && ls -1","description":"List the Workspace"},"shellToolInfo":{"displayCommand":"ls -1","hasWriteFileRedirection":false,"possiblePaths":[]}}'
      event e5 tool.execution_partial_result '{"toolCallId":"t1","partialOutput":"Cargo.toml\n"}'
      event e6 tool.execution_complete '{"toolCallId":"t1","success":true,"result":{"content":"Cargo.toml\nsrc\n"}}'
      event e7 tool.execution_start '{"toolCallId":"t2","toolName":"view","arguments":{"path":"Cargo.toml"}}'
      event e8 tool.execution_complete '{"toolCallId":"t2","success":false,"error":{"message":"Cargo.toml is not readable"}}'
      event e9 assistant.message '{"messageId":"m1","content":"Two entries."}'
      event e10 session.idle '{}'
"#;

/// A Turn Copilot abandons part-way through reasoning and running, leaving both open.
const ABANDONED_WORK: &str = r#"      event e1 assistant.reasoning_delta '{"reasoningId":"r1","deltaContent":"**Weighing it up**"}'
      event e2 tool.execution_start '{"toolCallId":"t1","toolName":"bash","arguments":{"command":"sleep 600"}}'
      event e3 session.error '{"errorType":"quota","message":"Out of premium requests."}'
      event e4 session.idle '{}'
"#;

/// The Session the Prompt `text` opened, once its first Turn has settled.
async fn worked_session(
    name: &'static str,
    timeline: &str,
    text: &str,
) -> suru::protocol::SessionSnapshot {
    let copilot = conversation_fixture(timeline);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), name).expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), name).await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let settled = settled_session(&client, created.session.id, 0).await;

    server.shutdown().await.expect("shut the server down");
    settled
}

#[tokio::test]
async fn copilots_reasoning_reaches_the_transcript_as_reasoning_activity() {
    let settled = worked_session(
        "copilot-reasoning",
        WORKING_TURN,
        "Tell me what is in the Workspace",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Some(Activity::Reasoning {
        id,
        status,
        title,
        content,
        content_truncated,
        duration_ms,
        ..
    }) = settled.activities.first()
    else {
        panic!(
            "Copilot's Reasoning projects as a Reasoning Activity, got {:?}",
            settled.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(title.as_deref(), Some("Reading the workspace"));
    assert_eq!(content, "Listing what is there.");
    assert!(!content_truncated);
    assert!(
        duration_ms.is_some(),
        "a block that settled by completing was timed over its own stretch of the Turn"
    );
    assert_eq!(
        settled
            .transcript
            .iter()
            .filter(
                |item| matches!(item, TranscriptItem::Activity { activity_id } if activity_id == id)
            )
            .count(),
        1,
        "Copilot's Reasoning streams into one Transcript row"
    );
}

#[tokio::test]
async fn copilots_tool_executions_reach_the_transcript_as_command_activity() {
    let settled = worked_session(
        "copilot-commands",
        WORKING_TURN,
        "Tell me what is in the Workspace",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&settled)[0].content, "Two entries.");
    let [_, ran, read] = settled.activities.as_slice() else {
        panic!(
            "the Turn's Reasoning and its two Tool executions are its Activities, got {:?}",
            settled.activities
        );
    };
    let Activity::Command {
        id,
        status,
        command,
        cwd,
        output,
        output_truncated,
        exit_status,
        ..
    } = ran
    else {
        panic!("a shell Tool execution projects as a Command Activity, got {ran:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        command, "ls -1",
        "the launcher plumbing Copilot rewrote is off the command a reader sees"
    );
    assert_eq!(
        *cwd, None,
        "every Copilot Tool runs in the Session's Workspace, which the Session already carries"
    );
    assert_eq!(output, "Cargo.toml\nsrc\n");
    assert!(!output_truncated);
    assert_eq!(*exit_status, None);
    assert_eq!(
        settled
            .transcript
            .iter()
            .filter(
                |item| matches!(item, TranscriptItem::Activity { activity_id } if activity_id == id)
            )
            .count(),
        1,
        "a command's output streams into one Transcript row"
    );

    let Activity::Command {
        status,
        command,
        output,
        ..
    } = read
    else {
        panic!("a Tool execution that runs no command still projects an Activity, got {read:?}");
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(command, r#"view {"path":"Cargo.toml"}"#);
    assert_eq!(
        output, "Cargo.toml is not readable",
        "a Tool that failed carries what Copilot said went wrong as its output"
    );
}

#[tokio::test]
async fn a_turn_that_stops_mid_work_leaves_nothing_running_in_the_transcript() {
    let settled = worked_session("copilot-abandoned-work", ABANDONED_WORK, "Spend the quota").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [reasoning, ran, failure] = settled.activities.as_slice() else {
        panic!(
            "the abandoned Reasoning and command settle beside the Turn's failure, got {:?}",
            settled.activities
        );
    };
    let Activity::Reasoning { status, title, .. } = reasoning else {
        panic!("the abandoned work is a Reasoning Activity, got {reasoning:?}");
    };
    assert_ne!(*status, ActivityStatus::Active, "no block is left open");
    assert_eq!(
        title.as_deref(),
        Some("Weighing it up"),
        "a block cut short keeps the title its split was still withholding"
    );
    let Activity::Command { status, .. } = ran else {
        panic!("the abandoned command is a Command Activity, got {ran:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "a command Copilot never reported finishing settles as failed"
    );
    let Activity::Error { text, .. } = failure else {
        panic!("the Turn's failure is an Error Activity, got {failure:?}");
    };
    assert_eq!(text, "Copilot quota error: Out of premium requests.");
}
