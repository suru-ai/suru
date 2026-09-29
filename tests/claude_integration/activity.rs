//! What Claude does while it works, in the Transcript: thinking streaming as Reasoning Activity
//! split at its headings, Bash executions recorded as Command Activity, and edits recorded as File
//! Changes, each settled by its tool result. A subagent's work is its own Session's — see
//! `subagents`.

use std::path::{Path, PathBuf};

use crate::support::{
    ScriptedClaude, agent_messages, conversation_arms, conversation_fixture, opened_session,
    opened_session_in, session_where, settled_session,
};
use serde_json::{Value, json};
use suru::protocol::{
    Activity, ActivityStatus, FileChange, SessionSnapshot, TranscriptItem, TurnStatus,
};

/// A Turn that thinks under a heading, runs one Bash command that succeeds and one that fails,
/// and answers. The commands exercise both input shapes the wire streams: input built up from
/// `input_json_delta` increments, and input carried whole on the block start.
const WORKING_TURN: &str = r#"      emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"**Reading the workspace**\n\nListing"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" what is there."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_ls","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" -1\",\"description\":\"List the Workspace\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ls","content":[{"type":"text","text":"Cargo.toml\nsrc\n"}],"is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_cat","name":"Bash","input":{"command":"cat missing.txt"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_cat","content":"cat: missing.txt: No such file or directory","is_error":true}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Two entries."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"num_turns":1,"result":"Two entries.","session_id":"prov-session"}'
"#;

/// A Turn whose one thinking block opens unheaded and then reaches a heading, so its split spans
/// an untitled block and a titled one.
const SPLIT_THINKING_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Sizing up the request."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"\n\n**Deciding**\n\nGoing with the simple route."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Done."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"Done.","session_id":"prov-session"}'
"#;

/// A Turn running one command wrapped in recognizable launcher plumbing and one in a shape the
/// stripping does not recognize.
const WRAPPED_COMMANDS_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_wrapped","name":"Bash","input":{"command":"bash -lc ls"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_wrapped","content":"src\n","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_odd","name":"Bash","input":{"command":"zsh -x ls"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_odd","content":"src\n","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":700,"num_turns":1,"result":"Ran.","session_id":"prov-session"}'
"#;

/// A Turn running one command that changes into a directory before its real work, and one that
/// changes directory in a way whose effect Suru cannot know.
const DIRECTORY_CHANGING_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_moved","name":"Bash","input":{"command":"cd /srv/app && cargo check"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_moved","content":"ok\n","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_relative","name":"Bash","input":{"command":"cd src && ls"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_relative","content":"main.rs\n","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":700,"num_turns":1,"result":"Ran.","session_id":"prov-session"}'
"#;

/// A Turn the CLI ends while a command still awaits its tool result.
const DANGLING_COMMAND_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_lost","name":"Bash","input":{"command":"sleep 600"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Gave up.","session_id":"prov-session"}'
"#;

/// A Turn whose one command answers with more output than Suru stores.
fn oversized_output_turn() -> String {
    let output = "x".repeat(70_000);
    format!(
        r#"      emit '{{"type":"stream_event","event":{{"type":"message_start","message":{{"role":"assistant"}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_big","name":"Bash","input":{{"command":"seq 1 100000"}}}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"message_stop"}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_big","content":"{output}","is_error":false}}]}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"result","subtype":"success","is_error":false,"duration_ms":600,"num_turns":1,"result":"Counted.","session_id":"prov-session"}}'
"#
    )
}

/// The Session the Prompt `text` opened, once its first Turn has settled.
async fn worked_session(name: &'static str, timeline: &str, text: &str) -> SessionSnapshot {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    worked_session_in(name, workspace, timeline, text).await
}

/// The same Session opened in `workspace`, which the test made.
async fn worked_session_in(
    name: &'static str,
    workspace: tempfile::TempDir,
    timeline: &str,
    text: &str,
) -> SessionSnapshot {
    let claude = conversation_fixture(timeline);
    let opened = opened_session_in(&claude, name, text, workspace).await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
    settled
}

#[tokio::test]
async fn claudes_thinking_reaches_the_transcript_as_reasoning_activity() {
    let settled = worked_session(
        "claude-reasoning",
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
            "Claude's thinking projects as a Reasoning Activity, got {:?}",
            settled.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        title.as_deref(),
        Some("Reading the workspace"),
        "a bold-headed block carries its heading as the typed title"
    );
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
        "Claude's thinking streams into one Transcript row"
    );
}

#[tokio::test]
async fn bash_executions_reach_the_transcript_as_command_activity() {
    let settled = worked_session(
        "claude-commands",
        WORKING_TURN,
        "Tell me what is in the Workspace",
    )
    .await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&settled)[0].content, "Two entries.");
    let [_, ran, failed] = settled.activities.as_slice() else {
        panic!(
            "the Turn's Reasoning and its two Bash executions are its Activities, got {:?}",
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
        panic!("a Bash execution projects as a Command Activity, got {ran:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        command, "ls -1",
        "Claude sends the command itself, and it is stored bare"
    );
    assert_eq!(
        cwd.as_deref(),
        Some(settled.session.execution_directory.path.as_path()),
        "a command that changes no directory runs where Claude's shell returns after every command"
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
    } = failed
    else {
        panic!("a failed Bash execution still projects a Command Activity, got {failed:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "a tool result reporting an error settles its command as failed"
    );
    assert_eq!(command, "cat missing.txt");
    assert_eq!(output, "cat: missing.txt: No such file or directory");
}

#[tokio::test]
async fn each_heading_in_a_thinking_block_begins_a_new_reasoning_block() {
    let settled = worked_session("claude-split-thinking", SPLIT_THINKING_TURN, "Decide").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [first, second] = settled.activities.as_slice() else {
        panic!(
            "the heading splits the thinking into two Reasoning Activities, got {:?}",
            settled.activities
        );
    };
    let Activity::Reasoning {
        status,
        title,
        content,
        ..
    } = first
    else {
        panic!("the unheaded lead-in is a Reasoning Activity, got {first:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(*title, None, "unheaded thinking yields an untitled block");
    assert_eq!(content, "Sizing up the request.");
    let Activity::Reasoning {
        status,
        title,
        content,
        ..
    } = second
    else {
        panic!("the headed remainder is a Reasoning Activity, got {second:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(title.as_deref(), Some("Deciding"));
    assert_eq!(content, "Going with the simple route.");
}

#[tokio::test]
async fn launcher_plumbing_is_stripped_and_unrecognized_shapes_stay_verbatim() {
    let settled = worked_session("claude-wrapped-commands", WRAPPED_COMMANDS_TURN, "Run").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [wrapped, odd] = settled.activities.as_slice() else {
        panic!(
            "both executions are Command Activities, got {:?}",
            settled.activities
        );
    };
    let Activity::Command { command, .. } = wrapped else {
        panic!("the wrapped execution is a Command Activity, got {wrapped:?}");
    };
    assert_eq!(
        command, "ls",
        "recognizable launcher plumbing is off the command a reader sees"
    );
    let Activity::Command { command, .. } = odd else {
        panic!("the unrecognized execution is a Command Activity, got {odd:?}");
    };
    assert_eq!(
        command, "zsh -x ls",
        "a shape the stripping does not recognize is recorded verbatim"
    );
}

#[tokio::test]
async fn a_leading_change_into_a_known_directory_becomes_the_commands_directory() {
    let settled = worked_session("claude-directory-change", DIRECTORY_CHANGING_TURN, "Run").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [moved, relative] = settled.activities.as_slice() else {
        panic!(
            "both executions are Command Activities, got {:?}",
            settled.activities
        );
    };
    let Activity::Command { command, cwd, .. } = moved else {
        panic!("the directory-changing execution is a Command Activity, got {moved:?}");
    };
    assert_eq!(
        (command.as_str(), cwd.as_deref()),
        ("cargo check", Some(std::path::Path::new("/srv/app"))),
        "the change into an absolute directory is where the command ran, not part of it"
    );
    let Activity::Command { command, cwd, .. } = relative else {
        panic!("the relative execution is a Command Activity, got {relative:?}");
    };
    assert_eq!(
        (command.as_str(), cwd.as_deref()),
        (
            "cd src && ls",
            Some(settled.session.execution_directory.path.as_path())
        ),
        "a relative change is kept in the command, which starts where every command does"
    );
}

/// Claude's shell otherwise keeps whatever directory a command changed into for every command
/// after it, so where a command ran would hang on every command before it.
#[tokio::test]
async fn a_sessions_claude_returns_its_shell_to_the_execution_directory_after_every_command() {
    let record = tempfile::tempdir().expect("create a directory for the launch record");
    let environments = record.path().join("environments");
    let claude = ScriptedClaude::with_preamble(
        &format!(
            "printf '%s\\t%s\\n' \"${{CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR-unset}}\" \"$*\" >> '{}'\n",
            environments.display()
        ),
        &conversation_arms(WORKING_TURN),
    );
    let opened = opened_session(&claude, "claude-maintained-directory", "Run").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");

    let recorded = std::fs::read_to_string(&environments).expect("read the launch record");
    let session_launch = recorded
        .lines()
        .find(|launch| launch.contains("--session-id"))
        .unwrap_or_else(|| panic!("the Session's CLI was launched, got {recorded:?}"));
    assert_eq!(
        session_launch.split_once('\t').map(|(value, _)| value),
        Some("1"),
        "the Session's CLI is told to return its shell to where it started after each command"
    );
}

#[tokio::test]
async fn a_command_whose_tool_result_never_came_settles_failed_when_the_turn_ends() {
    let settled = worked_session("claude-dangling-command", DANGLING_COMMAND_TURN, "Wait").await;

    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the CLI ended the Turn cleanly, and the dangling command does not hold it open"
    );
    let [command] = settled.activities.as_slice() else {
        panic!(
            "the dangling execution is the Turn's one Activity, got {:?}",
            settled.activities
        );
    };
    let Activity::Command { status, .. } = command else {
        panic!("the dangling execution is a Command Activity, got {command:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "a command with no outcome to match settles as failed rather than staying active"
    );
}

#[tokio::test]
async fn oversized_command_output_is_capped_and_marked_truncated() {
    let settled = worked_session("claude-truncation", &oversized_output_turn(), "Count").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [command] = settled.activities.as_slice() else {
        panic!(
            "the oversized execution is the Turn's one Activity, got {:?}",
            settled.activities
        );
    };
    let Activity::Command {
        output,
        output_truncated,
        ..
    } = command
    else {
        panic!("the oversized execution is a Command Activity, got {command:?}");
    };
    assert!(
        *output_truncated,
        "output cut short by the cap carries the typed Truncation property"
    );
    assert!(
        output.chars().count() <= 64 * 1024,
        "stored output stays within the cap, got {} chars",
        output.chars().count()
    );
    assert!(!output.is_empty(), "what fit under the cap is still stored");
}

/// One stream-json message as a fixture `emit` line. Timelines naming the fixture Workspace's own
/// paths are written from values rather than by hand, so every path is escaped as JSON escapes it
/// whatever platform rooted it.
fn emit(message: &Value) -> String {
    let line = message.to_string();
    assert!(
        !line.contains('\''),
        "an emitted line is single-quoted in the fixture, got {line}"
    );
    format!("      emit '{line}'\n")
}

/// A chunk of the loop's own streaming conversation.
fn chunk(event: Value) -> String {
    emit(&json!({
        "type": "stream_event",
        "event": event,
        "parent_tool_use_id": null,
        "session_id": "prov-session",
    }))
}

/// One assistant message of the loop's own using the tool `name` with `input` under the tool-use
/// id `id`: its input streams in an `input_json_delta`, and a full snapshot restates it after the
/// chunks, as the live CLI sends both.
fn tool_use(id: &str, name: &str, input: &Value) -> String {
    [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        chunk(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
        })),
        chunk(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()},
        })),
        chunk(json!({"type": "content_block_stop", "index": 0})),
        chunk(json!({"type": "message_stop"})),
        emit(&json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
            },
            "parent_tool_use_id": null,
            "session_id": "prov-session",
        })),
    ]
    .concat()
}

/// The tool result the loop echoes back for the tool use `id`, reporting an error when `is_error`.
fn tool_result(id: &str, content: &str, is_error: bool) -> String {
    emit(&json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": content,
                "is_error": is_error,
            }],
        },
        "parent_tool_use_id": null,
        "session_id": "prov-session",
    }))
}

/// The result ending the loop's Turn.
fn turn_result(text: &str) -> String {
    emit(&json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "duration_ms": 700,
        "num_turns": 1,
        "result": text,
        "session_id": "prov-session",
    }))
}

/// A Workspace the test owns, holding the file `existing` names so a write to it overwrites a file
/// that is really there.
fn workspace_holding(existing: &str) -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    std::fs::write(workspace.path().join(existing), "before\n")
        .expect("write the Workspace's existing file");
    workspace
}

/// Every Activity of `snapshot` as the File Change it must be — its status and the changes it
/// records — so a Tool use recorded as anything else, or recorded twice, fails the assertion.
fn file_changes(snapshot: &SessionSnapshot) -> Vec<(ActivityStatus, Vec<FileChange>)> {
    snapshot
        .activities
        .iter()
        .map(|activity| match activity {
            Activity::FileChange {
                status, changes, ..
            } => (*status, changes.clone()),
            other => panic!(
                "every Activity of an editing Turn is a File Change, got {other:?} among {:?}",
                snapshot.activities
            ),
        })
        .collect()
}

fn update(path: impl Into<PathBuf>) -> Vec<FileChange> {
    vec![FileChange::Update {
        path: path.into(),
        moved_to: None,
    }]
}

fn add(path: impl Into<PathBuf>) -> Vec<FileChange> {
    vec![FileChange::Add { path: path.into() }]
}

/// The path `name` in `workspace`, rooted as the platform roots it — the absolute path Claude
/// names a file by.
fn path_in(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(name)
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

#[tokio::test]
async fn claudes_edit_multi_edit_and_notebook_edit_reach_the_transcript_as_file_changes() {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let edited = path_in(workspace.path(), "lib.rs");
    let multi_edited = path_in(workspace.path(), "main.rs");
    let notebook = path_in(workspace.path(), "analysis.ipynb");
    let timeline = [
        tool_use(
            "toolu_edit",
            "Edit",
            &json!({
                "file_path": path_text(&edited),
                "old_string": "teh",
                "new_string": "the",
            }),
        ),
        tool_result(
            "toolu_edit",
            "The file has been updated successfully.",
            false,
        ),
        tool_use(
            "toolu_multi_edit",
            "MultiEdit",
            &json!({
                "file_path": path_text(&multi_edited),
                "edits": [
                    {"old_string": "foo", "new_string": "bar"},
                    {"old_string": "baz", "new_string": "qux"},
                ],
            }),
        ),
        tool_result("toolu_multi_edit", "Applied 2 edits to the file.", false),
        tool_use(
            "toolu_notebook_edit",
            "NotebookEdit",
            &json!({
                "notebook_path": path_text(&notebook),
                "cell_id": "cell-1",
                "new_source": "print(1)",
            }),
        ),
        tool_result("toolu_notebook_edit", "Updated cell cell-1.", false),
        turn_result("Edited."),
    ]
    .concat();

    let settled = worked_session_in("claude-edits", workspace, &timeline, "Fix the typos").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        file_changes(&settled),
        vec![
            (ActivityStatus::Completed, update(edited)),
            (ActivityStatus::Completed, update(multi_edited)),
            (ActivityStatus::Completed, update(notebook)),
        ],
        "each edit is one File Change updating the file it names, settled by its tool result"
    );
}

#[tokio::test]
async fn a_write_is_an_add_where_its_path_was_absent_as_it_opened_and_an_update_where_it_existed() {
    let workspace = workspace_holding("existing.txt");
    let created = path_in(workspace.path(), "created.txt");
    let overwritten = path_in(workspace.path(), "existing.txt");
    // The fixture holds once the first Write's tool use has closed, and only once released does
    // it create the file — as the Write running would — before its tool result arrives. So an Add
    // can only have been decided before the Write ran.
    let timeline = [
        tool_use(
            "toolu_create",
            "Write",
            &json!({"file_path": path_text(&created), "content": "fresh\n"}),
        ),
        "      while [ ! -e \"$CLAUDE_FIXTURE_RELEASE\" ]; do sleep 0.01; done\n".to_owned(),
        format!("      printf 'fresh\\n' > '{}'\n", path_text(&created)),
        tool_result("toolu_create", "File created successfully.", false),
        tool_use(
            "toolu_overwrite",
            "Write",
            &json!({"file_path": path_text(&overwritten), "content": "after\n"}),
        ),
        tool_result(
            "toolu_overwrite",
            "The file has been updated successfully.",
            false,
        ),
        turn_result("Written."),
    ]
    .concat();
    let claude = conversation_fixture(&timeline);
    let opened = opened_session_in(&claude, "claude-writes", "Write the files", workspace).await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the first Write opens its File Change",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::FileChange { .. }))
        },
    )
    .await;
    claude.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        file_changes(&settled),
        vec![
            (ActivityStatus::Completed, add(created)),
            (ActivityStatus::Completed, update(overwritten)),
        ],
        "a Write adds a file absent as its tool use opened, and updates one already there"
    );
}

#[tokio::test]
async fn a_relative_write_path_is_looked_up_where_claude_works() {
    let workspace = workspace_holding("existing.txt");
    let timeline = [
        tool_use(
            "toolu_relative",
            "Write",
            &json!({"file_path": "existing.txt", "content": "after\n"}),
        ),
        tool_result(
            "toolu_relative",
            "The file has been updated successfully.",
            false,
        ),
        turn_result("Written."),
    ]
    .concat();

    let settled =
        worked_session_in("claude-relative-write", workspace, &timeline, "Write it").await;

    assert_eq!(
        file_changes(&settled),
        vec![(ActivityStatus::Completed, update("existing.txt"))],
        "a relative path names a file in the execution directory, recorded as Claude named it"
    );
}

#[tokio::test]
async fn an_edit_whose_tool_result_reports_an_error_settles_its_file_change_as_failed() {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let edited = path_in(workspace.path(), "lib.rs");
    let written = path_in(workspace.path(), "denied.txt");
    let timeline = [
        tool_use(
            "toolu_edit",
            "Edit",
            &json!({
                "file_path": path_text(&edited),
                "old_string": "absent",
                "new_string": "present",
            }),
        ),
        tool_result(
            "toolu_edit",
            "<tool_use_error>String to replace not found in file.</tool_use_error>",
            true,
        ),
        tool_use(
            "toolu_write",
            "Write",
            &json!({"file_path": path_text(&written), "content": "nope\n"}),
        ),
        tool_result(
            "toolu_write",
            "<tool_use_error>Permission denied.</tool_use_error>",
            true,
        ),
        turn_result("Could not edit."),
    ]
    .concat();

    let settled = worked_session_in("claude-failed-edits", workspace, &timeline, "Edit").await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        file_changes(&settled),
        vec![
            (ActivityStatus::Failed, update(edited)),
            (ActivityStatus::Failed, add(written)),
        ],
        "a tool result reporting an error settles its File Change as failed"
    );
}

#[tokio::test]
async fn an_edit_whose_tool_result_never_came_settles_failed_when_the_turn_ends() {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let edited = path_in(workspace.path(), "lib.rs");
    let timeline = [
        tool_use(
            "toolu_lost",
            "Edit",
            &json!({
                "file_path": path_text(&edited),
                "old_string": "a",
                "new_string": "b",
            }),
        ),
        turn_result("Gave up."),
    ]
    .concat();

    let settled = worked_session_in("claude-dangling-edit", workspace, &timeline, "Edit").await;

    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the CLI ended the Turn cleanly, and the dangling edit does not hold it open"
    );
    assert_eq!(
        file_changes(&settled),
        vec![(ActivityStatus::Failed, update(edited))],
        "an edit with no outcome to match settles as failed rather than staying active"
    );
}

#[tokio::test]
async fn a_subagents_edit_is_a_file_change_in_the_subagents_own_session() {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let edited = path_in(workspace.path(), "lib.rs");
    let subagent_edit = json!({
        "file_path": path_text(&edited),
        "old_string": "teh",
        "new_string": "the",
    });
    let timeline = [
        tool_use(
            "task_edit",
            "Agent",
            &json!({
                "description": "Fix the typo",
                "prompt": "fix the typo",
                "subagent_type": "general-purpose",
            }),
        ),
        emit(&json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "agent-task-edit",
            "tool_use_id": "task_edit",
            "description": "Fix the typo",
            "task_type": "local_agent",
            "subagent_type": "general-purpose",
            "session_id": "prov-session",
        })),
        emit(&json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_sub_edit",
                    "name": "Edit",
                    "input": subagent_edit,
                }],
            },
            "parent_tool_use_id": "task_edit",
            "session_id": "prov-session",
        })),
        emit(&json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_sub_edit",
                    "content": "The file has been updated successfully.",
                    "is_error": false,
                }],
            },
            "parent_tool_use_id": "task_edit",
            "session_id": "prov-session",
        })),
        emit(&json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "agent-task-edit",
            "status": "completed",
            "summary": "Fixed it.",
            "session_id": "prov-session",
        })),
        tool_result("task_edit", "Fixed it.", false),
        turn_result("Fixed."),
    ]
    .concat();
    let claude = conversation_fixture(&timeline);
    let opened =
        opened_session_in(&claude, "claude-subagent-edit", "Fix the typo", workspace).await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
    ] = settled.activities.as_slice()
    else {
        panic!(
            "the Subagent row is all the parent's Transcript carries of its edit, got {:?}",
            settled.activities
        );
    };
    let child = settled_session(&opened.client, *child_id, 0).await;
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
    assert_eq!(
        file_changes(&child),
        vec![(ActivityStatus::Completed, update(edited))],
        "the subagent's edit lands in the Subagent's own Session"
    );
}
