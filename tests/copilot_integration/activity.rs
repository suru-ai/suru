//! What Copilot does while it works, in the Transcript: the Reasoning it reports as it reaches an
//! answer, its shell executions as Command Activity, and every other Tool execution no more
//! specific Activity records as a Tool Call.

use std::sync::Arc;

use crate::support::{
    agent_messages, connect, conversation_fixture, opened_session, session_where, settled_session,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, CreateSessionRequest, InitialPrompt, PromptId, SessionSnapshot,
        TranscriptItem, TurnStatus,
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

/// A Turn using Tools no more specific Activity records: an MCP server's Tool reporting progress
/// and streaming part of its output before its completion carries the rest, another answering
/// with text beside an image and a resource link, the Tools Copilot lists and reads its agents
/// with, and a built-in Tool that fails.
const TOOL_CALLS_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-issue","toolName":"github-create_issue","mcpServerName":"github","mcpToolName":"create_issue","arguments":{"title":"Fix the seam","labels":["bug"]}}'
      event e2 tool.execution_progress '{"toolCallId":"t-issue","progressMessage":"Contacting GitHub"}'
      event e3 tool.execution_partial_result '{"toolCallId":"t-issue","partialOutput":"Creating the issue\n"}'
      event e4 tool.execution_complete '{"toolCallId":"t-issue","success":true,"result":{"content":"Creating the issue\nCreated issue #7\n","contents":[{"type":"text","text":"Creating the issue\nCreated issue #7\n"}]}}'
      event e5 tool.execution_start '{"toolCallId":"t-shot","toolName":"browser-screenshot","mcpServerName":"browser","mcpToolName":"screenshot","arguments":{}}'
      event e6 tool.execution_complete '{"toolCallId":"t-shot","success":true,"result":{"content":"Captured the page","contents":[{"type":"text","text":"Captured the page"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"},{"type":"resource_link","uri":"file:///shot.png","name":"shot.png"}]}}'
      event e7 tool.execution_start '{"toolCallId":"t-list","toolName":"list_agents","arguments":{}}'
      event e8 tool.execution_complete '{"toolCallId":"t-list","success":true,"result":{"content":"No agents are running."}}'
      event e9 tool.execution_start '{"toolCallId":"t-read","toolName":"read_agent","arguments":{"agent_id":"scout","wait":true}}'
      event e10 tool.execution_complete '{"toolCallId":"t-read","success":true,"result":{"content":"Scout reported."}}'
      event e11 tool.execution_start '{"toolCallId":"t-fetch","toolName":"web_fetch","arguments":{"url":"https://example.com/missing"}}'
      event e12 tool.execution_partial_result '{"toolCallId":"t-fetch","partialOutput":"Fetching"}'
      event e13 tool.execution_complete '{"toolCallId":"t-fetch","success":false,"error":{"message":"Request failed with status code 404"}}'
      event e14 assistant.message '{"messageId":"m1","content":"Filed it."}'
      event e15 session.idle '{}'
"#;

/// A Turn using every Tool whose execution another Activity records or that is Copilot's
/// plumbing — a shell command, a question for the user, a spawn whose Subagent answers for it, a
/// message sent to that Subagent, the intent and completion reports — beside a Tool no other
/// Activity records, every one of them completed.
const ABSORBED_AND_PLUMBING_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-intent","toolName":"report_intent","arguments":{"intent":"Planning the work"}}'
      event e2 tool.execution_complete '{"toolCallId":"t-intent","success":true,"result":{"content":"Intent logged"}}'
      event e3 tool.execution_start '{"toolCallId":"t-bash","toolName":"bash","arguments":{"command":"ls"}}'
      event e4 tool.execution_complete '{"toolCallId":"t-bash","success":true,"result":{"content":"src\n"}}'
      event e5 tool.execution_start '{"toolCallId":"t-ask","toolName":"ask_user","arguments":{"question":"Which crate?","choices":["suru"]}}'
      event e6 tool.execution_complete '{"toolCallId":"t-ask","success":true,"result":{"content":"suru"}}'
      event e7 tool.execution_start '{"toolCallId":"t-spawn","toolName":"task","arguments":{"agent_type":"explore","name":"scout","description":"Scout the crate","prompt":"Scout the crate."}}'
      agent_event e8 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"scout","agentDisplayName":"Scout","agentDescription":"Scout the crate"}'
      agent_event e9 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"scout","agentDisplayName":"Scout"}'
      event e10 tool.execution_complete '{"toolCallId":"t-spawn","success":true,"result":{"content":"Scouted."}}'
      event e11 tool.execution_start '{"toolCallId":"t-write","toolName":"write_agent","arguments":{"agent_id":"agent-1","message":"And the tests."}}'
      event e12 tool.execution_complete '{"toolCallId":"t-write","success":true,"result":{"content":"Sent."}}'
      event e13 tool.execution_start '{"toolCallId":"t-grep","toolName":"grep","arguments":{"pattern":"TODO"}}'
      event e14 tool.execution_complete '{"toolCallId":"t-grep","success":true,"result":{"content":"src/main.rs:3"}}'
      event e15 tool.execution_start '{"toolCallId":"t-done","toolName":"task_complete","arguments":{"summary":"Planned it."}}'
      event e16 tool.execution_complete '{"toolCallId":"t-done","success":true,"result":{"content":"Planned it."}}'
      event e17 assistant.message '{"messageId":"m1","content":"Planned it."}'
      event e18 session.idle '{}'
"#;

/// A Turn whose Tools fail with a result beside the error: one reporting an empty result and
/// nothing streamed, one whose result repeats what it streamed, and one whose result already is
/// the error.
const FAILED_WITH_A_RESULT_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-denied","toolName":"web_fetch","arguments":{"url":"https://example.com/private"}}'
      event e2 tool.execution_complete '{"toolCallId":"t-denied","success":false,"result":{"content":""},"error":{"message":"Denied"}}'
      event e3 tool.execution_start '{"toolCallId":"t-slow","toolName":"web_fetch","arguments":{"url":"https://example.com/slow"}}'
      event e4 tool.execution_partial_result '{"toolCallId":"t-slow","partialOutput":"Fetching"}'
      event e5 tool.execution_complete '{"toolCallId":"t-slow","success":false,"result":{"content":"Fetching"},"error":{"message":"Timed out"}}'
      event e6 tool.execution_start '{"toolCallId":"t-refused","toolName":"web_fetch","arguments":{"url":"https://example.com/refused"}}'
      event e7 tool.execution_complete '{"toolCallId":"t-refused","success":false,"result":{"content":"Refused by policy"},"error":{"message":"Refused by policy"}}'
      event e8 assistant.message '{"messageId":"m1","content":"None of them loaded."}'
      event e9 session.idle '{}'
"#;

/// A Turn Copilot abandons while a Tool Call still runs.
const ABANDONED_TOOL_CALL: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t1","toolName":"web_search","arguments":{"query":"suru"}}'
      event e2 tool.execution_partial_result '{"toolCallId":"t1","partialOutput":"Searching"}'
      event e3 session.error '{"errorType":"quota","message":"Out of premium requests."}'
      event e4 session.idle '{}'
"#;

/// A Turn whose one Tool Call streams part of its output and holds, still running, until the test
/// releases it.
const HELD_TOOL_CALL_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t1","toolName":"glob","arguments":{"pattern":"**/*.rs"}}'
      event e2 tool.execution_partial_result '{"toolCallId":"t1","partialOutput":"src/lib.rs\n"}'
      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      event e3 tool.execution_complete '{"toolCallId":"t1","success":true,"result":{"content":"src/lib.rs\nsrc/main.rs\n"}}'
      event e4 assistant.message '{"messageId":"m1","content":"Two files."}'
      event e5 session.idle '{}'
"#;

/// A Turn whose one Tool Call is handed more input and streams more output than Suru stores.
fn oversized_tool_call_turn() -> String {
    let input = "y".repeat(5_000);
    let output = "x".repeat(35_000);
    format!(
        r#"      event e1 tool.execution_start '{{"toolCallId":"t1","toolName":"notes-save","mcpServerName":"notes","mcpToolName":"save","arguments":{{"content":"{input}"}}}}'
      event e2 tool.execution_partial_result '{{"toolCallId":"t1","partialOutput":"{output}"}}'
      event e3 tool.execution_partial_result '{{"toolCallId":"t1","partialOutput":"{output}"}}'
      event e4 tool.execution_complete '{{"toolCallId":"t1","success":true,"result":{{"content":"Saved."}}}}'
      event e5 assistant.message '{{"messageId":"m1","content":"Saved."}}'
      event e6 session.idle '{{}}'
"#
    )
}

/// The Tool Calls in `snapshot`, in Transcript order.
fn tool_calls(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::ToolCall { .. }))
        .collect()
}

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
                attachments: Vec::new(),
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
async fn copilots_shell_executions_are_commands_and_its_other_tools_tool_calls() {
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

    let Activity::ToolCall {
        status,
        name,
        server,
        input,
        output,
        ..
    } = read
    else {
        panic!("a Tool execution that runs no shell command is a Tool Call, got {read:?}");
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        (server.as_deref(), name.as_str()),
        (None, "view"),
        "a built-in Tool is named as Copilot names it"
    );
    assert_eq!(input, "path=Cargo.toml");
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

#[tokio::test]
async fn tool_executions_no_other_activity_records_reach_the_transcript_as_tool_calls() {
    let settled = worked_session("copilot-tool-calls", TOOL_CALLS_TURN, "File an issue").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&settled)[0].content, "Filed it.");
    let [issue, shot, list, read, fetch] = settled.activities.as_slice() else {
        panic!(
            "each Tool execution is one of the Turn's Activities, got {:?}",
            settled.activities
        );
    };
    let Activity::ToolCall {
        id,
        status,
        name,
        server,
        input,
        input_truncated,
        output,
        output_truncated,
        omitted_parts,
        ..
    } = issue
    else {
        panic!("an MCP Tool's execution is a Tool Call, got {issue:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (server.as_deref(), name.as_str()),
        (Some("github"), "create_issue"),
        "an MCP Tool is named by its server and its own name"
    );
    assert_eq!(input, r#"labels=["bug"] title=Fix the seam"#);
    assert!(!input_truncated);
    assert_eq!(
        output, "Creating the issue\nCreated issue #7\n",
        "the partial result streamed in, and the completion carried the rest of it — never the \
         progress message"
    );
    assert!(!output_truncated);
    assert_eq!(*omitted_parts, 0, "a text part is the output, not omitted");
    assert_eq!(
        settled
            .transcript
            .iter()
            .filter(
                |item| matches!(item, TranscriptItem::Activity { activity_id } if activity_id == id)
            )
            .count(),
        1,
        "a Tool Call opens, streams, and settles in one Transcript row"
    );

    let Activity::ToolCall {
        status,
        name,
        server,
        input,
        output,
        omitted_parts,
        ..
    } = shot
    else {
        panic!("an MCP Tool's execution is a Tool Call, got {shot:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (server.as_deref(), name.as_str()),
        (Some("browser"), "screenshot")
    );
    assert_eq!(input, "", "a Tool given no arguments shows no input");
    assert_eq!(
        output, "Captured the page",
        "only the result's text is stored"
    );
    assert_eq!(
        *omitted_parts, 2,
        "the image and the resource link beside the text are counted as omitted"
    );

    let recorded = [list, read]
        .into_iter()
        .map(|tool_call| match tool_call {
            Activity::ToolCall {
                status,
                name,
                server,
                input,
                output,
                ..
            } => (
                *status,
                server.as_deref(),
                name.as_str(),
                input.as_str(),
                output.as_str(),
            ),
            activity => panic!("an agent Tool's execution is a Tool Call, got {activity:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [
            (
                ActivityStatus::Completed,
                None,
                "list_agents",
                "",
                "No agents are running."
            ),
            (
                ActivityStatus::Completed,
                None,
                "read_agent",
                "agent_id=scout wait=true",
                "Scout reported."
            ),
        ],
        "listing and reading agents changes no Subagent row, so each is a Tool Call"
    );

    let Activity::ToolCall {
        status,
        name,
        output,
        ..
    } = fetch
    else {
        panic!("a built-in Tool's execution is a Tool Call, got {fetch:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "an execution Copilot reports failed settles its Tool Call as failed"
    );
    assert_eq!(name, "web_fetch");
    assert_eq!(
        output, "Fetching\nRequest failed with status code 404",
        "the error is the failed Tool Call's output, below what it had streamed"
    );
    assert!(
        settled
            .activities
            .iter()
            .all(|activity| !matches!(activity, Activity::Command { .. })),
        "no Tool that runs no shell command is a Command"
    );
}

#[tokio::test]
async fn tools_another_activity_records_and_copilots_plumbing_make_no_tool_call() {
    let settled = worked_session(
        "copilot-absorbed-tools",
        ABSORBED_AND_PLUMBING_TURN,
        "Plan the work",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let recorded = tool_calls(&settled)
        .into_iter()
        .map(|tool_call| match tool_call {
            Activity::ToolCall { name, output, .. } => (name.as_str(), output.as_str()),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [("grep", "src/main.rs:3")],
        "the search is a Tool Call, while report_intent, bash, ask_user, task, write_agent and \
         task_complete are not: {:?}",
        settled.activities
    );
    let commands = settled
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Command { command, .. } => Some(command.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(commands, ["ls"], "the shell execution stays a Command");
    let subagents = settled
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .count();
    assert_eq!(
        subagents, 1,
        "the spawn stands only in the Subagent row it opened"
    );
    assert_eq!(
        settled.activities.len(),
        3,
        "nothing else stands in the Transcript: {:?}",
        settled.activities
    );
}

#[tokio::test]
async fn a_tool_call_streams_its_output_while_it_runs() {
    let copilot = conversation_fixture(HELD_TOOL_CALL_TURN);
    let opened = opened_session(&copilot, "copilot-held-tool-call", "Find the sources").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");

    let running = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the running Tool Call shows what it has streamed",
        |snapshot| {
            tool_calls(snapshot).iter().any(|tool_call| {
                matches!(tool_call, Activity::ToolCall { output, .. } if !output.is_empty())
            })
        },
    )
    .await;
    let [
        Activity::ToolCall {
            status,
            name,
            input,
            output,
            ..
        },
    ] = tool_calls(&running)[..]
    else {
        panic!(
            "the running execution is one Tool Call, got {:?}",
            running.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Active, "the Tool is still running");
    assert_eq!(name, "glob");
    assert_eq!(
        input, "pattern=**/*.rs",
        "the input is known from the start"
    );
    assert_eq!(output, "src/lib.rs\n");

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    let [Activity::ToolCall { status, output, .. }] = tool_calls(&settled)[..] else {
        panic!(
            "the settled execution is one Tool Call, got {:?}",
            settled.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(output, "src/lib.rs\nsrc/main.rs\n");
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_tool_call_copilot_never_reported_finishing_settles_failed_with_its_turn() {
    let settled =
        worked_session("copilot-abandoned-tool-call", ABANDONED_TOOL_CALL, "Search").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [Activity::ToolCall { status, output, .. }] = tool_calls(&settled)[..] else {
        panic!(
            "the abandoned search is one Tool Call, got {:?}",
            settled.activities
        );
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "a Tool Call left running settles as failed rather than staying active"
    );
    assert_eq!(output, "Searching", "what it streamed is kept");
}

#[tokio::test]
async fn oversized_tool_call_input_and_output_are_capped_and_marked_truncated() {
    let settled = worked_session(
        "copilot-tool-call-truncation",
        &oversized_tool_call_turn(),
        "Save the notes",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [
        Activity::ToolCall {
            name,
            server,
            input,
            input_truncated,
            output,
            output_truncated,
            ..
        },
    ] = tool_calls(&settled)[..]
    else {
        panic!(
            "the oversized execution is one Tool Call, got {:?}",
            settled.activities
        );
    };
    assert_eq!((server.as_deref(), name.as_str()), (Some("notes"), "save"));
    assert!(
        *input_truncated,
        "input cut short by the cap carries the typed Truncation property"
    );
    assert!(
        input.chars().count() <= 4 * 1024,
        "stored input stays within a few KB, got {} chars",
        input.chars().count()
    );
    assert!(input.starts_with("content=yyy"), "what fit is still stored");
    assert!(
        *output_truncated,
        "output cut short by the cap carries the typed Truncation property"
    );
    assert!(
        output.chars().count() <= 64 * 1024,
        "stored output stays within a command's cap, got {} chars",
        output.chars().count()
    );
    assert!(
        output.starts_with("xxx"),
        "what fit under the cap is still stored"
    );
}

#[tokio::test]
async fn a_failed_tool_calls_error_is_its_output_even_beside_a_result() {
    let settled = worked_session(
        "copilot-failed-tool-call-with-result",
        FAILED_WITH_A_RESULT_TURN,
        "Fetch the pages",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let recorded = tool_calls(&settled)
        .into_iter()
        .map(|tool_call| match tool_call {
            Activity::ToolCall { status, output, .. } => (*status, output.as_str()),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [
            (ActivityStatus::Failed, "Denied"),
            (ActivityStatus::Failed, "Fetching\nTimed out"),
            (ActivityStatus::Failed, "Refused by policy"),
        ],
        "an empty result leaves the error as the whole output; a result repeating the stream \
         leaves the error below what streamed; an error the result already reports is not \
         repeated"
    );
}
