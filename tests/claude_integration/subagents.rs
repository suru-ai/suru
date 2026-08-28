//! Claude's task lifecycle as Subagents: a Task spawn opens the inline row and the child Session,
//! every `parent_tool_use_id`-attributed event lands in the child's Transcript, a subagent
//! outliving the Turn keeps running, its late output streams into a Continuation, and nested
//! spawns recurse one level down.

use crate::support::{
    agent_messages, conversation_fixture, opened_session, session_where, settled_session,
};
use suru::protocol::{Activity, ActivityStatus, SessionSnapshot, TurnStatus};

/// A fan-out running in the foreground of the Turn: the conversation spawns a subagent through
/// the Task tool, the CLI reports the task starting with the spawning tool use's identity, the
/// subagent narrates, thinks, and runs Bash — every chunk attributed by `parent_tool_use_id` —
/// and the CLI notifies the task settling before the conversation answers.
const FAN_OUT_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_1","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Scout the workspace\",\"prompt\":\"scout the workspace\",\"subagent_type\":\"Explore\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"task_1","description":"Scout the workspace","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Scouting the workspace now."}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"","signature":""}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"**Scouting plan**\n\nLook for TODO markers."}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_sub","name":"Bash","input":{}}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"rg -l TODO\"}"}},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":2},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sub","content":"src/main.rs\n","is_error":false}]},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-task-1","status":"completed","summary":"Found one file.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_1","content":"Found one file.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"One TODO file."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
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
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Explore", "the row names the subagent's own type");
    assert_eq!(description, "Scout the workspace");
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
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_audit","name":"Bash","input":{"command":"cargo audit"}}},"parent_tool_use_id":"task_bg","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_bg","session_id":"prov-session"}'
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
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_inner","name":"Task","input":{}}},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-inner","tool_use_id":"task_inner","description":"Map the callers","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Twelve call sites."}},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-inner","status":"completed","summary":"Mapped.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_inner","content":"Twelve call sites.","is_error":false}]},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Refactor in two steps."}},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
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
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_inner","name":"Task","input":{}}},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_outer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-inner","tool_use_id":"task_inner","description":"Map the callers","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-outer","status":"killed","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Twelve call sites."}},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":"task_inner","session_id":"prov-session"}'
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
