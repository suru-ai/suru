//! Claude's task lifecycle as Subagents: a Task spawn opens the inline row and the child Session,
//! every `parent_tool_use_id`-attributed message lands in the child's Transcript, a subagent
//! outliving the Turn keeps running, its late output streams into a Continuation, and nested
//! spawns recurse one level down.
//!
//! The fixtures mirror the live 2.1.280 CLI: only the loop's own conversation streams in
//! `stream_event` chunks (with full `assistant` snapshots restating them), while a subagent's
//! conversation arrives as those snapshots alone — full messages carrying `parent_tool_use_id`,
//! never chunks.

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{
        CLAUDE_MODELS, ScriptedClaude, after_probe, agent_messages, conversation_fixture,
        discovery_arms, interrupt_arm, opened_session, session_where, settled_session,
        stop_task_arm, user_turn_arm,
    },
};
use suru::{
    managed_client::SubagentTreeEvent,
    protocol::{
        Activity, ActivityStatus, Delegator, MessageRole, SessionSnapshot, TranscriptItem, TurnId,
        TurnStatus,
    },
};
use tokio::time::timeout;

/// A fan-out running in the foreground of the Turn: the conversation spawns a subagent through
/// the Task tool, the CLI reports the task starting with the spawning tool use's identity, the
/// subagent narrates, thinks, and runs Bash — arriving as full snapshots attributed by
/// `parent_tool_use_id` — the CLI ticks the task's progress with the subagent's latest tool
/// activity, and notifies the task settling before the conversation answers.
/// The loop's own answer streams with its snapshot restating it beside the chunks.
const FAN_OUT_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_1","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Scout the workspace\",\"prompt\":\"scout the workspace\",\"subagent_type\":\"Explore\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"task_1","description":"Scout the workspace","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg-child-1","role":"assistant","model":"claude-sonnet-child","content":[{"type":"text","text":"Scouting the workspace now."}]},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"**Scouting plan**\n\nLook for TODO markers.","signature":"sig"},{"type":"tool_use","id":"toolu_sub","name":"Bash","input":{"command":"rg -l TODO"}}]},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sub","content":"src/main.rs\n","is_error":false}]},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_progress","task_id":"agent-task-1","tool_use_id":"task_1","description":"Running rg -l TODO","subagent_type":"Explore","usage":{"total_tokens":1200,"tool_uses":1,"duration_ms":900},"last_tool_name":"Bash","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-task-1","status":"completed","summary":"Found one file.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_1","content":"Found one file.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"One TODO file."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"One TODO file."}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":2100,"num_turns":1,"result":"One TODO file.","session_id":"prov-session"}'
"#;

/// The one Subagent row in `snapshot`.
fn the_subagent_row(snapshot: &SessionSnapshot) -> &Activity {
    let mut rows = snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }));
    let row = rows.next().unwrap_or_else(|| {
        panic!(
            "the Transcript carries a Subagent row, got {:?}",
            snapshot.activities
        )
    });
    assert!(
        rows.next().is_none(),
        "the Transcript carries exactly one Subagent row, got {:?}",
        snapshot.activities
    );
    row
}

#[tokio::test]
async fn a_task_spawn_opens_the_row_and_the_child_session_that_holds_the_subagents_work() {
    let claude = conversation_fixture(FAN_OUT_TURN);
    let opened = opened_session(&claude, "claude-subagent-fan-out", "Scout for TODOs").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [message] = agent_messages(&settled)[..] else {
        panic!(
            "only the conversation's own Message reaches the parent, got {:?}",
            settled.messages
        );
    };
    assert_eq!(message.content, "One TODO file.");
    let Activity::Subagent {
        status,
        name,
        description,
        session_id: child_id,
        duration_ms,
        model,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Explore", "the row names the subagent's own type");
    assert_eq!(
        description, "Scout the workspace",
        "a task_progress tick leaves the row reading what the subagent was asked to do"
    );
    assert_eq!(
        model.as_ref().map(|model| model.as_str()),
        Some("claude-sonnet-child")
    );
    assert!(
        duration_ms.is_some(),
        "the settled row states how long the delegation ran"
    );
    assert_eq!(
        settled.activities.len(),
        1,
        "the row is all the parent's Transcript carries of the subagent: {:?}",
        settled.activities
    );

    let child = settled_session(client, *child_id, 0).await;
    assert_eq!(
        child.session.parent,
        Some(session_id),
        "the child names the Session whose Turn spawned it"
    );
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        child.turns[0]
            .agent
            .as_ref()
            .map(|agent| agent.selection.model.as_str()),
        Some("claude-sonnet-child")
    );
    let [narration] = agent_messages(&child)[..] else {
        panic!(
            "the subagent's narration is the child's Message, got {:?}",
            child.messages
        );
    };
    assert_eq!(narration.content, "Scouting the workspace now.");
    let [reasoning, command] = child.activities.as_slice() else {
        panic!(
            "the subagent's thinking and Bash execution are the child's Activities, got {:?}",
            child.activities
        );
    };
    let Activity::Reasoning {
        status,
        title,
        content,
        ..
    } = reasoning
    else {
        panic!("the subagent's thinking is Reasoning Activity, got {reasoning:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(title.as_deref(), Some("Scouting plan"));
    assert_eq!(content, "Look for TODO markers.");
    let Activity::Command {
        status,
        command,
        output,
        ..
    } = command
    else {
        panic!("the subagent's Bash execution is Command Activity, got {command:?}");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(command, "rg -l TODO");
    assert_eq!(output, "src/main.rs\n");

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn whose subagent outlives it: the spawn and the subagent's Bash execution stream, the
/// conversation's own result ends the Turn, and the fixture holds until the test has read that
/// state. Released, the subagent's work settles, the CLI notifies the task's end, and the loop
/// wakes to deliver the outcome — output belonging to no Turn the user began.
const OUTLIVING_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_bg","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"task_bg","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_audit","name":"Bash","input":{"command":"cargo audit"}}]},"parent_tool_use_id":"task_bg","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":400,"num_turns":1,"result":"Kicked off the audit.","session_id":"prov-session"}'
      while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_audit","content":"0 vulnerabilities\n","is_error":false}]},"parent_tool_use_id":"task_bg","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_updated","task_id":"agent-task-bg","patch":{"status":"running","description":"Summarizing the audit"},"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-task-bg","status":"completed","summary":"All clear.","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The audit found nothing."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The audit found nothing."}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":80,"num_turns":1,"result":"The audit found nothing.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_subagent_outliving_the_turn_keeps_running_and_its_late_output_begins_a_continuation() {
    let claude = conversation_fixture(OUTLIVING_TURN);
    let opened = opened_session(
        &claude,
        "claude-subagent-outlives",
        "Audit the dependencies",
    )
    .await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    // The Turn settled at the CLI's own boundary while the subagent works on.
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        status,
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the row works on past the Turn's settle"
    );
    let child_id = *child_id;
    let mut child_feed = client
        .subscribe_session(child_id)
        .await
        .expect("subscribe to the child Session");
    let child = session_where(
        client,
        &mut child_feed,
        child_id,
        "the subagent's command reaches the child's Transcript",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;
    let [Activity::Command { status, .. }] = child.activities.as_slice() else {
        panic!(
            "the subagent's execution is the child's one Activity, got {:?}",
            child.activities
        );
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the parent's result no longer settles the subagent's command as failed"
    );

    claude.release();
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    let [Activity::Command { status, output, .. }] = child.activities.as_slice() else {
        panic!(
            "the subagent's execution settles in the child, got {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(output, "0 vulnerabilities\n");

    // The output the subagent's completion provoked streams into a Continuation: a second Turn
    // no Prompt began, settled at the CLI's next boundary.
    let with_continuation = settled_session(client, session_id, 1).await;
    assert_eq!(with_continuation.turns.len(), 2);
    let continuation = &with_continuation.turns[1];
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation is the one Turn without a Prompt"
    );
    assert_eq!(continuation.status, TurnStatus::Completed);
    let late = agent_messages(&with_continuation);
    let [message] = late.as_slice() else {
        panic!(
            "the late output is the Session's one agent Message, got {:?}",
            with_continuation.messages
        );
    };
    assert_eq!(message.content, "The audit found nothing.");
    assert_eq!(
        message.turn_id, continuation.id,
        "the late Message belongs to the Continuation"
    );
    let Activity::Subagent {
        status,
        description,
        duration_ms,
        ..
    } = the_subagent_row(&with_continuation)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        description, "Summarizing the audit",
        "a task_updated patch revises what the row says the subagent is doing"
    );
    assert!(duration_ms.is_some());

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A spawn inside a spawn: the subagent runs its own Task tool use, the grandchild narrates under
/// the nested tool use's identity, and each level settles from the inside out.
const NESTED_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_outer","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-outer","tool_use_id":"task_outer","description":"Plan the refactor","task_type":"local_agent","subagent_type":"Plan","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"task_inner","name":"Task","input":{}}]},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-inner","tool_use_id":"task_inner","description":"Map the callers","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Twelve call sites."}]},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-inner","status":"completed","summary":"Mapped.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_inner","content":"Twelve call sites.","is_error":false}]},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Refactor in two steps."}]},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-outer","status":"completed","summary":"Planned.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_outer","content":"Refactor in two steps.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Here is the plan."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"Here is the plan.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_subagent_spawning_a_subagent_records_the_grandchild_one_level_down() {
    let claude = conversation_fixture(NESTED_TURN);
    let opened = opened_session(&claude, "claude-subagent-nested", "Plan the refactor").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        status,
        name,
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Plan");

    let child = settled_session(client, *child_id, 0).await;
    assert_eq!(child.session.parent, Some(session_id));
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&child)[0].content, "Refactor in two steps.");
    let Activity::Subagent {
        status,
        name,
        description,
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Explore");
    assert_eq!(description, "Map the callers");

    let grandchild = settled_session(client, *grandchild_id, 0).await;
    assert_eq!(
        grandchild.session.parent,
        Some(*child_id),
        "the grandchild is the child's own child, not the parent's"
    );
    assert_eq!(grandchild.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&grandchild)[0].content, "Twelve call sites.");

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A spawner stopped out from under its child: the outer task's notification arrives while the
/// inner still runs — the wire promises no inside-out order — and the inner's own settle follows.
const OUTER_SETTLES_FIRST_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_outer","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-outer","tool_use_id":"task_outer","description":"Plan the refactor","task_type":"local_agent","subagent_type":"Plan","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"task_inner","name":"Task","input":{}}]},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-inner","tool_use_id":"task_inner","description":"Map the callers","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-outer","status":"killed","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Twelve call sites."}]},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-inner","status":"completed","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_outer","content":"Stopped.","is_error":true}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"The planner was stopped.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_outer_settle_does_not_orphan_the_inner_subagent_it_leaves_running() {
    let claude = conversation_fixture(OUTER_SETTLES_FIRST_TURN);
    let opened = opened_session(&claude, "claude-subagent-outer-first", "Plan the refactor").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    let Activity::Subagent {
        status,
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "a stopped task settles its row as failed"
    );

    let child = settled_session(client, *child_id, 0).await;
    let Activity::Subagent {
        status,
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Completed,
        "the inner settle still reaches its row after the spawner's own"
    );
    let grandchild = settled_session(client, *grandchild_id, 0).await;
    assert_eq!(grandchild.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&grandchild)[0].content, "Twelve call sites.");

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn holding a Subagent in flight and never answering on its own: the spawn streams, and the
/// conversation stays open until an interrupt ends it.
const SUBAGENT_IN_FLIGHT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_1","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"task_1","description":"Scout the workspace","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
"#;

/// What the CLI writes once the interrupt has stopped its loop.
const INTERRUPTED_RESULT: &str = r#"      emit '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":11,"num_turns":1,"errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],"terminal_reason":"aborted_streaming","session_id":"prov-session"}'
"#;

/// The one control request of `subtype` the fixture captured.
fn request_named(claude: &ScriptedClaude, subtype: &str) -> serde_json::Value {
    claude
        .requests()
        .into_iter()
        .find(|request| request.pointer("/request/subtype") == Some(&subtype.into()))
        .unwrap_or_else(|| panic!("a `{subtype}` control request reaches the CLI"))
}

#[tokio::test]
async fn interrupting_a_turn_stops_its_working_subagent_before_the_loop() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(SUBAGENT_IN_FLIGHT),
        stop_task_arm(),
        interrupt_arm(INTERRUPTED_RESULT),
    ));
    let opened = opened_session(&claude, "claude-interrupt-reaches-subagents", "Fan out").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    let spawned = session_where(
        client,
        &mut feed,
        session_id,
        "the Subagent's row reaches the Transcript",
        |snapshot| matches!(snapshot.activities.first(), Some(Activity::Subagent { .. })),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&spawned)
    else {
        unreachable!()
    };
    let child_id = *child_id;

    client
        .interrupt_session(session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let interrupted = settled_session(client, session_id, 0).await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = the_subagent_row(&interrupted)
    else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "the stopped Subagent's row settles as stopped, not failed"
    );
    assert!(
        duration_ms.is_some(),
        "the stop is a real settle, so the row states how long the delegation ran"
    );
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);

    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task", "interrupt"]),
        "the Subagent is stopped before the loop is, in the established ordering"
    );
    assert_eq!(
        request_named(&claude, "stop_task").pointer("/request/task_id"),
        Some(&"agent-task-1".into()),
        "the stop names the task running the Subagent"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn that spawns a Subagent and completes at once, leaving only the Subagent working; the
/// conversation then stays open until the interrupt's stop reaches it.
const SUBAGENT_OUTLIVES_THE_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_bg","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"task_bg","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":400,"num_turns":1,"result":"Kicked off the audit.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn interrupting_with_no_turn_active_stops_the_outliving_subagent_without_an_interrupt() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(SUBAGENT_OUTLIVES_THE_TURN),
        stop_task_arm(),
    ));
    let opened = opened_session(&claude, "claude-idle-interrupt-subagents", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        status,
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Active);
    let child_id = *child_id;

    client
        .interrupt_session(session_id)
        .await
        .expect("the interrupt works with no Turn active");

    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    let stopped = session_where(
        client,
        &mut feed,
        session_id,
        "the stopped Subagent's row settles",
        |snapshot| {
            matches!(
                the_subagent_row(snapshot),
                Activity::Subagent {
                    status: ActivityStatus::Interrupted,
                    ..
                }
            )
        },
    )
    .await;
    let Activity::Subagent { duration_ms, .. } = the_subagent_row(&stopped) else {
        unreachable!()
    };
    assert!(duration_ms.is_some());
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(
        child.turns[0].status,
        TurnStatus::Interrupted,
        "the stop settles the child's Turn, which is what clears Working"
    );

    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task"]),
        "with no loop running there is nothing to interrupt: the stop alone goes out"
    );
    assert_eq!(
        request_named(&claude, "stop_task").pointer("/request/task_id"),
        Some(&"agent-task-bg".into()),
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn that fans out to two Subagents and completes, leaving both working.
const TWO_SUBAGENTS_OUTLIVE_THE_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_1","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"task_1","description":"Scout the workspace","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"task_2","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-2","tool_use_id":"task_2","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":400,"num_turns":1,"result":"Fanned out.","session_id":"prov-session"}'
"#;

/// The Subagent rows in `snapshot`, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .collect()
}

#[tokio::test]
async fn stopping_one_subagent_by_its_session_leaves_the_other_working() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(TWO_SUBAGENTS_OUTLIVE_THE_TURN),
        stop_task_arm(),
    ));
    let opened = opened_session(&claude, "claude-stop-one-subagent", "Fan out").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;
    let [
        Activity::Subagent {
            session_id: first_child,
            ..
        },
        Activity::Subagent { .. },
    ] = subagent_rows(&settled)[..]
    else {
        panic!(
            "both spawns reach the Transcript, got {:?}",
            settled.activities
        );
    };
    let first_child = *first_child;

    // Interrupting the Subagent's own Session is the Picker row's stop.
    client
        .interrupt_session(first_child)
        .await
        .expect("the Subagent's Session accepts the stop");

    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    let after = session_where(
        client,
        &mut feed,
        session_id,
        "the stopped Subagent's row settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [
                    Activity::Subagent {
                        status: ActivityStatus::Interrupted,
                        ..
                    },
                    ..
                ]
            )
        },
    )
    .await;
    let [_, Activity::Subagent { status, .. }] = subagent_rows(&after)[..] else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the other Subagent works on: the stop reached one delegation, not the Session"
    );
    let stopped_child = settled_session(client, first_child, 0).await;
    assert_eq!(stopped_child.turns[0].status, TurnStatus::Interrupted);

    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task"]),
        "one stop goes out, and no interrupt follows it"
    );
    assert_eq!(
        request_named(&claude, "stop_task").pointer("/request/task_id"),
        Some(&"agent-task-1".into()),
        "the stop names the task running the Subagent that was asked to stop"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// The resume of a settled background agent through SendMessage, as the live 2.1.280 CLI reports
/// it: the loop spawns the agent and its Turn ends once the agent has said HELLO and settled; the
/// loop wakes on its own to send it more, and that SendMessage starts the same task again, naming
/// the SendMessage tool use as its `tool_use_id` and repeating the spawn's description, while the
/// resumed agent's conversation still rides under the original Agent tool use as
/// `parent_tool_use_id`. The resumed stretch runs on another Model than the first, so each
/// stretch's Model evidence is told apart.
const RESUMED_SUBAGENT_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_1","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Say hello\",\"prompt\":\"Say HELLO.\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"agent_1","content":"Async agent launched successfully.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"HELLO"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","status":"completed","summary":"Agent \"Say hello\" completed","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Launched.","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"send_1","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"a2046dbbe8ecd4a5c\",\"message\":\"Now say GOODBYE.\\nNothing else.\",\"summary\":\"Say goodbye\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"send_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"send_1","content":"{\"success\":true,\"message\":\"Resuming agent a2046db\"}","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-resumed","content":[{"type":"text","text":"GOODBYE"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_progress","task_id":"a2046dbbe8ecd4a5c","description":"Saying goodbye","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"send_1","status":"completed","summary":"Agent \"Say hello\" completed","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"DONE"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"DONE","session_id":"prov-session"}'
"#;

/// The Model a Turn's Agent was observed running, if any.
fn turn_model(turn: &suru::protocol::Turn) -> Option<&str> {
    turn.agent
        .as_ref()
        .map(|agent| agent.selection.model.as_str())
}

#[tokio::test]
async fn a_subagent_resumed_through_send_message_continues_in_its_own_session_as_a_second_turn() {
    let claude = conversation_fixture(RESUMED_SUBAGENT_TURN);
    let opened = opened_session(&claude, "claude-subagent-resumed", "Hello then goodbye").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let parent = settled_session(client, session_id, 1).await;

    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            status: spawn_status,
            description: spawn_description,
            model: spawn_model,
            session_id: spawn_child,
            duration_ms: spawn_duration,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            status: resume_status,
            name: resume_name,
            description: resume_description,
            model: resume_model,
            session_id: resume_child,
            duration_ms: resume_duration,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the spawn and the resume each stand as a row, got {:?}",
            parent.activities
        );
    };
    assert_eq!(
        parent.turns.len(),
        2,
        "the loop woke on its own to resume the agent"
    );
    assert_eq!(*spawn_turn, parent.turns[0].id);
    assert_eq!(
        *resume_turn, parent.turns[1].id,
        "the resume row stands in the Turn that delegated the resume"
    );
    assert_eq!(
        resume_child, spawn_child,
        "both rows lead into the Subagent's one Session"
    );
    assert_eq!(*spawn_status, ActivityStatus::Completed);
    assert_eq!(spawn_description, "Say hello");
    assert_eq!(
        spawn_model.as_ref().map(|model| model.as_str()),
        Some("claude-haiku-child"),
        "the spawn's row keeps what its own stretch settled with"
    );
    assert!(spawn_duration.is_some());
    assert_eq!(*resume_status, ActivityStatus::Completed);
    assert_eq!(resume_name, "general-purpose");
    assert_eq!(
        resume_description, "Say goodbye",
        "the resume reads SendMessage's summary, not the spawn's description the start repeats"
    );
    assert_eq!(
        resume_model.as_ref().map(|model| model.as_str()),
        Some("claude-sonnet-resumed"),
        "the resumed stretch's snapshots are the resume row's Model evidence"
    );
    assert!(resume_duration.is_some());

    let child = settled_session(client, *spawn_child, 1).await;
    assert_eq!(
        child.title, "Say hello",
        "a resume leaves the Title as it was"
    );
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the resume begins a second Turn in the one child Session, got {:?}",
            child.turns
        );
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(second.status, TurnStatus::Completed);
    assert_eq!(turn_model(first), Some("claude-haiku-child"));
    assert_eq!(
        turn_model(second),
        Some("claude-sonnet-resumed"),
        "the resumed stretch's Model evidence belongs to the Turn it began"
    );
    let [hello, goodbye] = agent_messages(&child)[..] else {
        panic!(
            "the child holds the agent's whole conversation, got {:?}",
            child.messages
        );
    };
    assert_eq!(hello.content, "HELLO");
    assert_eq!(hello.turn_id, first.id);
    assert_eq!(goodbye.content, "GOODBYE");
    assert_eq!(
        goodbye.turn_id, second.id,
        "the resumed work lands in the Turn the resume began"
    );

    let mut tree = client.subscribe_subagent_tree(session_id);
    let Some(SubagentTreeEvent::Snapshot(tree)) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree arrives")
    else {
        panic!("the Subagent tree subscription opens with its snapshot");
    };
    let [entry] = tree.subagents.as_slice() else {
        panic!(
            "the resumed agent is one entry in the tree, got {:?}",
            tree.subagents
        );
    };
    assert_eq!(entry.session_id, *spawn_child);
    assert_eq!(entry.parent_session_id, session_id);
    assert_eq!(entry.title, "Say hello");
    assert_eq!(
        (entry.status, entry.worked_ms),
        (
            ActivityStatus::Completed,
            Some(
                child
                    .turns
                    .iter()
                    .map(|turn| {
                        turn.settled_at.expect("the Turn settled").0
                            - turn.started_at.expect("the Turn began").0
                    })
                    .sum()
            )
        ),
        "the entry wears the resume's outcome, its time summed over both Turns"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// The Messages in `snapshot`'s Transcript, in order: each one's Turn, who it is from — the user,
/// the Agent, or the Agent a Delegation names — and what it says.
fn transcript_messages(snapshot: &SessionSnapshot) -> Vec<(TurnId, &MessageRole, &str)> {
    snapshot
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Message { message_id } => snapshot
                .messages
                .iter()
                .find(|message| message.id == *message_id),
            TranscriptItem::Activity { .. } => None,
        })
        .map(|message| (message.turn_id, &message.role, message.content.as_str()))
        .collect()
}

#[tokio::test]
async fn the_spawn_and_the_resume_each_open_their_turn_in_the_child_with_their_delegation() {
    let claude = conversation_fixture(RESUMED_SUBAGENT_TURN);
    let opened = opened_session(&claude, "claude-subagent-delegations", "Hello then goodbye").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let parent = settled_session(client, session_id, 1).await;
    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
        _,
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the spawn and the resume each stand as a row, got {:?}",
            parent.activities
        );
    };
    let child = settled_session(client, *child_id, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!("the resume begins a second Turn, got {:?}", child.turns);
    };
    let from_parent = MessageRole::Delegation(Delegator {
        session_id,
        name: None,
    });
    assert_eq!(
        transcript_messages(&child),
        [
            (first.id, &from_parent, "Say HELLO."),
            (first.id, &MessageRole::Agent, "HELLO"),
            (second.id, &from_parent, "Now say GOODBYE.\nNothing else."),
            (second.id, &MessageRole::Agent, "GOODBYE"),
        ],
        "the first Turn opens with the Agent tool's prompt and the second with SendMessage's \
         message, each a Delegation from the parent's Agent ahead of the work it began"
    );
    assert!(
        child
            .messages
            .iter()
            .all(|message| message.role != MessageRole::User),
        "a Delegation is never a user Message: {:?}",
        child.messages
    );
    assert!(
        parent
            .messages
            .iter()
            .all(|message| message.role.delegator().is_none()),
        "the Delegations stand in the Subagent's Transcript alone"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// The same resume, but the resumed agent is still working when the loop's stretch that sent the
/// SendMessage ends, and it works on until something stops it.
const RESUMED_SUBAGENT_STILL_WORKING: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_1","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"HELLO"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","status":"completed","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Launched.","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"send_1","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"a2046dbbe8ecd4a5c\",\"message\":\"Keep saying GOODBYE until told to stop.\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"send_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"GOODBYE"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":200,"num_turns":1,"result":"Resumed.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_resumed_subagent_keeps_its_parent_working_and_stopping_it_settles_only_its_resumed_turn()
{
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(RESUMED_SUBAGENT_STILL_WORKING),
        stop_task_arm(),
    ));
    let opened = opened_session(
        &claude,
        "claude-stop-resumed-subagent",
        "Hello then goodbye",
    )
    .await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    let resumed = session_where(
        client,
        &mut feed,
        session_id,
        "the resumed agent works on past the stretch that resumed it",
        |snapshot| {
            snapshot.turns.len() == 2
                && snapshot.turns[1].status != TurnStatus::Active
                && matches!(
                    subagent_rows(snapshot)[..],
                    [
                        _,
                        Activity::Subagent {
                            status: ActivityStatus::Active,
                            ..
                        }
                    ]
                )
        },
    )
    .await;
    assert!(
        resumed.session.working_since.is_some(),
        "the resumed Subagent keeps its parent Working once every Turn has settled (ADR 0015)"
    );
    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
        Activity::Subagent {
            description,
            session_id: resumed_child,
            ..
        },
    ] = subagent_rows(&resumed)[..]
    else {
        unreachable!()
    };
    assert_eq!(resumed_child, child_id);
    assert_eq!(
        description, "Keep saying GOODBYE until told to stop.",
        "a SendMessage with no summary is described by its message's first line"
    );
    let child_id = *child_id;
    let mut child_feed = client
        .subscribe_session(child_id)
        .await
        .expect("subscribe to the child Session");
    session_where(
        client,
        &mut child_feed,
        child_id,
        "the resumed work reaches the child's second Turn",
        |snapshot| agent_messages(snapshot).len() == 2,
    )
    .await;

    // Interrupting the Subagent's own Session is the stop its row and the Subagent Picker offer.
    client
        .interrupt_session(child_id)
        .await
        .expect("the resumed Subagent's Session accepts the stop");

    let stopped = session_where(
        client,
        &mut feed,
        session_id,
        "the resume row settles as stopped and the parent stops Working",
        |snapshot| {
            snapshot.session.working_since.is_none()
                && matches!(
                    subagent_rows(snapshot)[..],
                    [
                        _,
                        Activity::Subagent {
                            status: ActivityStatus::Interrupted,
                            ..
                        }
                    ]
                )
        },
    )
    .await;
    let [
        Activity::Subagent {
            status: spawn_status,
            ..
        },
        Activity::Subagent { duration_ms, .. },
    ] = subagent_rows(&stopped)[..]
    else {
        unreachable!()
    };
    assert_eq!(
        *spawn_status,
        ActivityStatus::Completed,
        "the spawn's row stays exactly as it settled"
    );
    assert!(
        duration_ms.is_some(),
        "the stop settles the resume row's own stretch"
    );
    let child = settled_session(client, child_id, 1).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        child.turns[1].status,
        TurnStatus::Interrupted,
        "the stop settles the Turn the resume began"
    );

    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task"]),
    );
    assert_eq!(
        request_named(&claude, "stop_task").pointer("/request/task_id"),
        Some(&"a2046dbbe8ecd4a5c".into()),
        "the stop names the resumed task, the Subagent's own identity"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A sibling resuming a settled Subagent: the loop spawns a writer and a reviewer, the writer
/// settles, and the reviewer — whose conversation arrives as snapshots — sends the writer more
/// work through SendMessage, so the writer's task starts again naming the reviewer's tool use.
/// The writer's Agent tool input never streams, so only its task's start says what it was asked.
const SIBLING_RESUMES_SUBAGENT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_writer","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"writer-task","tool_use_id":"agent_writer","description":"Draft the notes","prompt":"Draft the release notes.","task_type":"local_agent","subagent_type":"Writer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"agent_reviewer","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"reviewer-task","tool_use_id":"agent_reviewer","description":"Review the notes","task_type":"local_agent","subagent_type":"Reviewer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Drafted."}]},"parent_tool_use_id":"agent_writer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"writer-task","status":"completed","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"send_review","name":"SendMessage","input":{"to":"writer-task","message":"Tighten the second paragraph.","summary":"Tighten the notes"}}]},"parent_tool_use_id":"agent_reviewer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"writer-task","tool_use_id":"send_review","description":"Draft the notes","task_type":"local_agent","subagent_type":"Writer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Tightened."}]},"parent_tool_use_id":"agent_writer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"writer-task","status":"completed","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"reviewer-task","status":"completed","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"Reviewed.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_sibling_that_resumes_a_subagent_holds_the_resume_row_in_its_own_turn() {
    let claude = conversation_fixture(SIBLING_RESUMES_SUBAGENT);
    let opened = opened_session(&claude, "claude-sibling-resumes", "Write then review").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let parent = settled_session(client, session_id, 0).await;

    let [
        Activity::Subagent {
            session_id: writer,
            name: writer_name,
            ..
        },
        Activity::Subagent {
            session_id: reviewer,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the parent holds only the two spawns, got {:?}",
            parent.activities
        );
    };
    assert_eq!(writer_name, "Writer");
    let reviewer_session = settled_session(client, *reviewer, 0).await;
    let Activity::Subagent {
        status,
        name,
        description,
        session_id: resumed,
        turn_id,
        ..
    } = the_subagent_row(&reviewer_session)
    else {
        unreachable!()
    };
    assert_eq!(
        resumed, writer,
        "the reviewer's row leads into the writer's own Session"
    );
    assert_eq!(*turn_id, reviewer_session.turns[0].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Writer");
    assert_eq!(description, "Tighten the notes");

    let writer_session = settled_session(client, *writer, 1).await;
    assert_eq!(
        writer_session.session.parent,
        Some(session_id),
        "the writer still hangs where it first spawned"
    );
    let [drafted, tightened] = agent_messages(&writer_session)[..] else {
        panic!(
            "the writer's Session holds both stretches, got {:?}",
            writer_session.messages
        );
    };
    assert_eq!(drafted.turn_id, writer_session.turns[0].id);
    assert_eq!(tightened.content, "Tightened.");
    assert_eq!(tightened.turn_id, writer_session.turns[1].id);
    let delegations = transcript_messages(&writer_session)
        .into_iter()
        .filter(|(_, role, _)| role.delegator().is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        delegations,
        [
            (
                writer_session.turns[0].id,
                &MessageRole::Delegation(Delegator {
                    session_id,
                    name: None,
                }),
                "Draft the release notes."
            ),
            (
                writer_session.turns[1].id,
                &MessageRole::Delegation(Delegator {
                    session_id: *reviewer,
                    name: Some("Reviewer".to_owned()),
                }),
                "Tighten the second paragraph."
            ),
        ],
        "the spawn's Delegation falls back to the prompt its task's start carries, and the \
         sibling's resume is attributed to the sibling that sent it"
    );

    let mut tree = client.subscribe_subagent_tree(session_id);
    let Some(SubagentTreeEvent::Snapshot(tree)) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree arrives")
    else {
        panic!("the Subagent tree subscription opens with its snapshot");
    };
    assert_eq!(
        tree.subagents
            .iter()
            .map(|entry| (entry.name.as_str(), entry.parent_session_id))
            .collect::<Vec<_>>(),
        [("Writer", session_id), ("Reviewer", session_id)],
        "the writer is listed once, where it first spawned, not again under the reviewer"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A SendMessage resuming an agent the Session holds no record of — spawned before its identity was
/// stored, say — whose conversation rides under a spawn this wire never saw.
const UNPLACED_RESUME_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"send_1","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"a0ld0ld0ld0ld0ld\",\"message\":\"Pick up where you left off.\",\"summary\":\"Pick up again\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a0ld0ld0ld0ld0ld","tool_use_id":"send_1","description":"Old work","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"Picking up."}]},"parent_tool_use_id":"agent_before_the_record","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a0ld0ld0ld0ld0ld","tool_use_id":"send_1","status":"completed","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"DONE"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":400,"num_turns":1,"result":"DONE","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_resume_of_an_agent_the_session_holds_no_record_of_opens_a_subagent_holding_its_work() {
    let claude = conversation_fixture(UNPLACED_RESUME_TURN);
    let opened = opened_session(&claude, "claude-subagent-unplaced", "Pick the old agent up").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(parent.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        turn_id,
        status,
        name,
        description,
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*turn_id, parent.turns[0].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "general-purpose");
    assert_eq!(description, "Pick up again");

    let child = settled_session(&opened.client, *child_id, 0).await;
    assert_eq!(child.session.parent, Some(opened.session_id));
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        transcript_messages(&child),
        [
            (
                child.turns[0].id,
                &MessageRole::Delegation(Delegator {
                    session_id: opened.session_id,
                    name: None,
                }),
                "Pick up where you left off.",
            ),
            (child.turns[0].id, &MessageRole::Agent, "Picking up."),
        ],
        "the resume's Delegation opens the Subagent it was recorded as, ahead of its work"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
