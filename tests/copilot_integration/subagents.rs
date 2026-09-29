//! Copilot's sub-agent events as Subagents: `subagent.started` opens the inline row and the child
//! Session, every event the envelope attributes to the sub-agent lands in the child's Transcript
//! — no longer failing the Session as the main agent's second Message — a Subagent outliving the
//! Turn keeps running past `session.idle`, and its late output streams into a Continuation.

use crate::support::{
    ScriptedCopilot, abort_arm, agent_messages, conversation_arms, conversation_fixture,
    opened_session, send_arm, session_where, settled_session,
};
use suru::protocol::{
    Activity, ActivityStatus, Delegator, MessageRole, SessionSnapshot, TranscriptItem, TurnStatus,
};

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

/// A delegation running in the foreground of the Turn: the main agent's Message is still open
/// when the sub-agent starts, narrates, reasons, and runs a Tool — every event stamped with its
/// envelope attribution — and the sub-agent completes before the main agent finishes its answer.
const FAN_OUT_TURN: &str = r#"      event e1 assistant.message_start '{"messageId":"m1"}'
      event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Delegating to the researcher."}'
      agent_event e3 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}'
      agent_event e-usage agent-1 assistant.usage '{"model":"gpt-child","inputTokens":20,"outputTokens":5}'
      agent_event e4 agent-1 assistant.message_start '{"messageId":"sub-m1"}'
      agent_event e5 agent-1 assistant.message_delta '{"messageId":"sub-m1","deltaContent":"Scouting the workspace now."}'
      agent_event e6 agent-1 assistant.message '{"messageId":"sub-m1","content":"Scouting the workspace now."}'
      agent_event e7 agent-1 assistant.reasoning_delta '{"reasoningId":"sub-r1","deltaContent":"**Scouting plan**\n\nLook for TODO markers."}'
      agent_event e8 agent-1 assistant.reasoning '{"reasoningId":"sub-r1","content":"**Scouting plan**\n\nLook for TODO markers."}'
      agent_event e9 agent-1 tool.execution_start '{"toolCallId":"t-sub","toolName":"bash","arguments":{"command":"rg -l TODO"}}'
      agent_event e10 agent-1 tool.execution_partial_result '{"toolCallId":"t-sub","partialOutput":"src/main.rs\n"}'
      agent_event e11 agent-1 tool.execution_complete '{"toolCallId":"t-sub","success":true,"result":{"content":"src/main.rs\n"}}'
      agent_event e12 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","durationMs":1200}'
      event e13 assistant.message_delta '{"messageId":"m1","deltaContent":" One TODO file."}'
      event e14 assistant.message '{"messageId":"m1","content":"Delegating to the researcher. One TODO file."}'
      event e15 session.idle '{}'
"#;

#[tokio::test]
async fn attributed_events_land_in_the_child_session_while_the_parents_transcript_stays_clean() {
    let copilot = conversation_fixture(FAN_OUT_TURN);
    let opened = opened_session(&copilot, "copilot-subagent-fan-out", "Scout for TODOs").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [message] = agent_messages(&settled)[..] else {
        panic!(
            "only the main agent's own Message reaches the parent — a sub-agent's interleaved \
             Message neither lands there nor fails the Session, got {:?}",
            settled.messages
        );
    };
    assert_eq!(
        message.content,
        "Delegating to the researcher. One TODO file."
    );
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
    assert_eq!(name, "Researcher");
    assert_eq!(description, "Scout the workspace");
    assert_eq!(
        model.as_ref().map(|model| model.as_str()),
        Some("gpt-child")
    );
    assert!(
        duration_ms.is_some(),
        "the settled row states how long the delegation ran"
    );
    assert_eq!(
        settled.activities.len(),
        1,
        "the row is all the parent's Transcript carries of the sub-agent: {:?}",
        settled.activities
    );

    let child = settled_session(client, *child_id, 0).await;
    assert_eq!(
        child.turns[0]
            .agent
            .as_ref()
            .map(|agent| agent.selection.model.as_str()),
        Some("gpt-child")
    );
    assert_eq!(
        child.session.parent,
        Some(session_id),
        "the child names the Session whose Turn spawned it"
    );
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    let [narration] = agent_messages(&child)[..] else {
        panic!(
            "the sub-agent's narration is the child's Message, got {:?}",
            child.messages
        );
    };
    assert_eq!(narration.content, "Scouting the workspace now.");
    let [reasoning, command] = child.activities.as_slice() else {
        panic!(
            "the sub-agent's reasoning and Tool execution are the child's Activities, got {:?}",
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
        panic!("the sub-agent's reasoning is Reasoning Activity, got {reasoning:?}");
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
        panic!("the sub-agent's Tool execution is Command Activity, got {command:?}");
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

/// A delegation made through the task tool, as the SDK's own captures show it: the main agent's
/// `task` execution starts carrying the prompt it hands the sub-agent, `subagent.started` names
/// that tool call — but never the prompt — and the sub-agent answers before the execution settles.
const TASK_TOOL_SPAWN_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-spawn","toolName":"task","arguments":{"agent_type":"explore","name":"read-file","description":"Reading notes.txt","prompt":"Read notes.txt and report its contents."}}'
      agent_event e2 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"explore","agentDisplayName":"Explore","agentDescription":"Reading notes.txt"}'
      agent_event e3 agent-1 assistant.message '{"messageId":"sub-m1","content":"The notes say hello."}'
      agent_event e4 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"explore","agentDisplayName":"Explore"}'
      event e5 tool.execution_complete '{"toolCallId":"t-spawn","success":true,"result":{"content":"The notes say hello."}}'
      event e6 assistant.message '{"messageId":"m1","content":"The notes say hello."}'
      event e7 session.idle '{}'
"#;

#[tokio::test]
async fn a_task_tool_spawn_opens_its_child_turn_with_the_prompt_as_a_delegation() {
    let copilot = conversation_fixture(TASK_TOOL_SPAWN_TURN);
    let opened = opened_session(&copilot, "copilot-subagent-delegation", "Read the notes").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };

    let child = settled_session(client, *child_id, 0).await;
    let messages = child
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Message { message_id } => child
                .messages
                .iter()
                .find(|message| message.id == *message_id),
            TranscriptItem::Activity { .. } => None,
        })
        .map(|message| (message.turn_id, &message.role, message.content.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            (
                child.turns[0].id,
                &MessageRole::Delegation(Delegator {
                    session_id,
                    name: None,
                }),
                "Read notes.txt and report its contents."
            ),
            (
                child.turns[0].id,
                &MessageRole::Agent,
                "The notes say hello."
            ),
        ],
        "the task tool's prompt opens the child's Turn as a Delegation from the main agent"
    );
    assert!(
        settled
            .messages
            .iter()
            .all(|message| message.role.delegator().is_none()),
        "the Delegation stands in the Subagent's Transcript alone"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn whose sub-agent outlives it: the loop goes idle while the sub-agent's Tool still runs,
/// and the fixture holds until the test has read that state. Released, the sub-agent's work
/// settles, and the output its completion provokes belongs to no Turn the user began.
const OUTLIVING_TURN: &str = r#"      event e1 assistant.message '{"messageId":"m1","content":"Kicked off the audit."}'
      agent_event e2 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"auditor","agentDisplayName":"Auditor","agentDescription":"Audit the dependencies"}'
      agent_event e3 agent-1 tool.execution_start '{"toolCallId":"t-audit","toolName":"bash","arguments":{"command":"cargo audit"}}'
      event e4 session.idle '{}'
      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      agent_event e5 agent-1 tool.execution_complete '{"toolCallId":"t-audit","success":true,"result":{"content":"0 vulnerabilities\n"}}'
      agent_event e6 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"auditor","agentDisplayName":"Auditor","durationMs":900}'
      event e7 assistant.message '{"messageId":"m2","content":"The audit found nothing."}'
      event e8 session.idle '{}'
"#;

#[tokio::test]
async fn a_subagent_outliving_the_turn_keeps_running_and_its_late_output_begins_a_continuation() {
    let copilot = conversation_fixture(OUTLIVING_TURN);
    let opened = opened_session(
        &copilot,
        "copilot-subagent-outlives",
        "Audit the dependencies",
    )
    .await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    // The Turn settled at Copilot's own idle boundary while the sub-agent works on.
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
        "the sub-agent's command reaches the child's Transcript",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;
    let [Activity::Command { status, .. }] = child.activities.as_slice() else {
        panic!(
            "the sub-agent's execution is the child's one Activity, got {:?}",
            child.activities
        );
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the idle settles the Turn, not the sub-agent's still-running command"
    );

    copilot.release();
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    let [Activity::Command { status, output, .. }] = child.activities.as_slice() else {
        panic!(
            "the sub-agent's execution settles in the child, got {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(output, "0 vulnerabilities\n");

    // The output the sub-agent's completion provoked streams into a Continuation: a second Turn
    // no Prompt began, settled at the loop's next idle.
    let with_continuation = settled_session(client, session_id, 1).await;
    assert_eq!(with_continuation.turns.len(), 2);
    let continuation = &with_continuation.turns[1];
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation is the one Turn without a Prompt"
    );
    assert_eq!(continuation.status, TurnStatus::Completed);
    let late = agent_messages(&with_continuation);
    let [first, message] = late.as_slice() else {
        panic!(
            "the late output joins the Turn's own Message in the Session, got {:?}",
            with_continuation.messages
        );
    };
    assert_eq!(first.content, "Kicked off the audit.");
    assert_eq!(message.content, "The audit found nothing.");
    assert_eq!(
        message.turn_id, continuation.id,
        "the late Message belongs to the Continuation"
    );
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = the_subagent_row(&with_continuation)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert!(
        duration_ms.is_some(),
        "the settled row states how long the delegation ran"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A delegation that dies partway: the sub-agent narrates, then `subagent.failed` reports it
/// failing, and the main agent answers around the failure.
const FAILING_TURN: &str = r#"      agent_event e1 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}'
      agent_event e2 agent-1 assistant.message '{"messageId":"sub-m1","content":"Partway through."}'
      agent_event e3 agent-1 subagent.failed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","error":"the researcher crashed"}'
      event e4 assistant.message '{"messageId":"m1","content":"The researcher failed."}'
      event e5 session.idle '{}'
"#;

#[tokio::test]
async fn a_failed_subagent_settles_its_row_and_its_child_turn_as_failed() {
    let copilot = conversation_fixture(FAILING_TURN);
    let opened = opened_session(&copilot, "copilot-subagent-failed", "Scout for TODOs").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;

    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the sub-agent failing is its own outcome, not the Turn's"
    );
    let Activity::Subagent {
        status,
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Failed);

    let child = settled_session(client, *child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert_eq!(
        agent_messages(&child)[0].content,
        "Partway through.",
        "what the sub-agent did before failing stays in the child's Transcript"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A Turn that hands work to a sub-agent and goes idle at once, leaving only the delegation
/// running.
const SUBAGENT_OUTLIVES_THE_TURN: &str = r#"      event e1 assistant.message '{"messageId":"m1","content":"Kicked off the audit."}'
      agent_event e2 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"auditor","agentDisplayName":"Auditor","agentDescription":"Audit the dependencies"}'
      event e3 session.idle '{}'
"#;

#[tokio::test]
async fn interrupting_with_no_turn_active_aborts_the_loop_and_settles_the_subagent_as_stopped() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(SUBAGENT_OUTLIVES_THE_TURN),
        abort_arm(""),
    ));
    let opened = opened_session(&copilot, "copilot-idle-interrupt-subagents", "Audit").await;
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
    session_where(
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
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(
        child.turns[0].status,
        TurnStatus::Interrupted,
        "the stop settles the child's Turn, which is what clears Working"
    );
    assert!(
        copilot
            .methods()
            .iter()
            .any(|method| method == "session.abort"),
        "with no per-sub-agent stop, stopping the delegations is the whole-loop abort"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_per_subagent_stop_is_refused_and_leaves_the_delegation_running() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}",
        conversation_arms(),
        send_arm(SUBAGENT_OUTLIVES_THE_TURN),
    ));
    let opened = opened_session(&copilot, "copilot-subagent-stop-refused", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };

    let refused = client
        .interrupt_session(*child_id)
        .await
        .expect_err("Copilot offers no per-Subagent stop in this cut");
    assert!(
        refused.to_string().contains("no per-Subagent stop"),
        "the refusal says why, got: {refused:#}"
    );

    let unchanged = client
        .read_session(session_id)
        .await
        .expect("read the Session after the refusal");
    let Activity::Subagent { status, .. } = the_subagent_row(&unchanged) else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "a refused stop stops nothing"
    );
    assert!(
        !copilot
            .methods()
            .iter()
            .any(|method| method == "session.abort"),
        "the refusal never reaches the CLI: one Subagent is not the whole loop"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A delegation whose sub-agent reads a file — a Tool its envelope attributes to it — before it
/// completes and the main agent answers.
const SUBAGENT_TOOL_CALL_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-spawn","toolName":"task","arguments":{"agent_type":"explore","name":"reader","description":"Read the manifest","prompt":"Read the manifest."}}'
      agent_event e2 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"reader","agentDisplayName":"Reader","agentDescription":"Read the manifest"}'
      agent_event e3 agent-1 tool.execution_start '{"toolCallId":"t-sub-view","toolName":"view","arguments":{"path":"Cargo.toml"}}'
      agent_event e4 agent-1 tool.execution_complete '{"toolCallId":"t-sub-view","success":true,"result":{"content":"[package]\n"}}'
      agent_event e5 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"reader","agentDisplayName":"Reader"}'
      event e6 tool.execution_complete '{"toolCallId":"t-spawn","success":true,"result":{"content":"Read it."}}'
      event e7 assistant.message '{"messageId":"m1","content":"Read it."}'
      event e8 session.idle '{}'
"#;

/// A spawn Copilot refuses before any sub-agent starts, so no Subagent row ever answers for it.
const REFUSED_SPAWN_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-spawn","toolName":"task","arguments":{"agent_type":"no-such-agent","prompt":"Scout."}}'
      event e2 tool.execution_complete '{"toolCallId":"t-spawn","success":false,"error":{"message":"Unknown agent type: no-such-agent"}}'
      event e3 assistant.message '{"messageId":"m1","content":"No such agent."}'
      event e4 session.idle '{}'
"#;

#[tokio::test]
async fn a_subagents_tool_calls_land_in_its_own_session_never_its_parents() {
    let copilot = conversation_fixture(SUBAGENT_TOOL_CALL_TURN);
    let opened = opened_session(&copilot, "copilot-subagent-tool-call", "Read the manifest").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(
        settled.activities.len(),
        1,
        "the row is all the parent's Transcript carries of the sub-agent: {:?}",
        settled.activities
    );

    let child = settled_session(&opened.client, *child_id, 0).await;
    let [
        Activity::ToolCall {
            status,
            name,
            server,
            input,
            output,
            ..
        },
    ] = child.activities.as_slice()
    else {
        panic!(
            "the sub-agent's read is the child's one Activity, a Tool Call, got {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!((server.as_deref(), name.as_str()), (None, "view"));
    assert_eq!(input, "path=Cargo.toml");
    assert_eq!(output, "[package]\n");
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_spawn_no_subagent_answers_for_surfaces_as_a_failed_tool_call() {
    let copilot = conversation_fixture(REFUSED_SPAWN_TURN);
    let opened = opened_session(&copilot, "copilot-refused-spawn", "Scout").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let [
        Activity::ToolCall {
            status,
            name,
            input,
            output,
            ..
        },
    ] = settled.activities.as_slice()
    else {
        panic!(
            "the refused delegation stays visible as the Turn's one Activity, a Tool Call rather \
             than a Command, got {:?}",
            settled.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(name, "task");
    assert_eq!(input, "agent_type=no-such-agent prompt=Scout.");
    assert_eq!(output, "Unknown agent type: no-such-agent");
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
