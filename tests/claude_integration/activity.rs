//! What Claude does while it works, in the Transcript: thinking streaming as Reasoning Activity
//! split at its headings, and Bash executions recorded as Command Activity settled by their tool
//! results. A subagent's work is its own Session's — see `subagents`.

use crate::support::{agent_messages, conversation_fixture, opened_session, settled_session};
use suru::protocol::{Activity, ActivityStatus, SessionSnapshot, TranscriptItem, TurnStatus};

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
    let claude = conversation_fixture(timeline);
    let opened = opened_session(&claude, name, text).await;
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
        *cwd, None,
        "every Bash execution runs in the Session's Workspace, which the Session already carries"
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
