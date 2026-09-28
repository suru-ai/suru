//! Codex collab threads as Subagents: a spawn item on the parent thread opens the inline row and
//! a child Session, Suru attaches the child thread so its own items stream into that Session, a
//! child completion arriving after the parent's turn completed settles the row rather than
//! erroring, a child's own spawns recurse one level down, a delegation that starts another turn on
//! a settled child's thread resumes it in its own Session — across a restart too — and one a
//! working child drains into its running turn steers that Turn instead.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::{Value, json};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SubagentTreeEvent},
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, ApprovalOutcome, CreateSessionRequest,
        Decision, Delegator, InitialPrompt, MessageRole, PromptDelivery, PromptId, SessionId,
        SessionSnapshot, TranscriptItem, Turn, TurnId, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::timeout;

/// A server hosting the scripted Codex, a client connected past its initial state, and a Session
/// opened on `prompt`, with the directories the Session lives in held for the fixture's lifetime.
/// `name` is the client channel, so each test needs its own.
struct OpenedSession {
    server: server::RunningServer,
    client: ManagedClient,
    session_id: SessionId,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

async fn opened_session(codex: &ScriptedCodex, name: &'static str, prompt: &str) -> OpenedSession {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), name).expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: prompt.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    OpenedSession {
        server,
        client,
        session_id: created.session.id,
        _state_dir: state_dir,
        _workspace: workspace,
    }
}

const CHILD_APPROVAL_IMMEDIATE_TERMINAL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo check","cwd":"project","status":"inProgress"}}}'
      printf '%s\n' '{"id":"child-approval","method":"item/commandExecution/requestApproval","params":{"threadId":"child-thread","turnId":"child-turn","itemId":"child-command","reason":"finish the delegated check","command":"cargo check"}}'
      ;;
    *'"id":"child-approval","result"'*)
      $TERMINAL
      ;;
"#;

#[tokio::test]
async fn immediate_native_child_terminal_notifications_wait_for_decision_history() {
    for (label, terminal) in [
        (
            "activity",
            r#"printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'"#,
        ),
        (
            "collab",
            r#"printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-wait","tool":"wait","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"agentsStates":{"child-thread":{"status":"completed"}}}}}'"#,
        ),
    ] {
        let fixture = ScriptedCodex::new_multiprocess(
            &CHILD_APPROVAL_IMMEDIATE_TERMINAL_CODEX.replace("$TERMINAL", terminal),
        );
        let opened = opened_session(
            &fixture,
            match label {
                "activity" => "codex-child-decision-activity-terminal",
                "collab" => "codex-child-decision-collab-terminal",
                _ => unreachable!(),
            },
            "Delegate a check",
        )
        .await;
        let parent = session_where(
            &opened.client,
            opened.session_id,
            "the child opens",
            |snapshot| matches!(snapshot.activities.first(), Some(Activity::Subagent { .. })),
        )
        .await;
        let Activity::Subagent {
            session_id: child_id,
            ..
        } = the_subagent_row(&parent)
        else {
            unreachable!()
        };
        let child_id = *child_id;
        let pending = session_where(
            &opened.client,
            child_id,
            "the child's Approval arrives",
            |snapshot| snapshot.pending_approvals.len() == 1,
        )
        .await;
        let Activity::Approval { approval, .. } = pending
            .activities
            .iter()
            .find(|activity| matches!(activity, Activity::Approval { .. }))
            .expect("the child owns its Approval")
        else {
            unreachable!()
        };

        opened
            .client
            .submit_decision(child_id, approval.id, Decision::Accept)
            .await
            .expect("Codex accepts the child Decision");

        let settled_child = session_where(
            &opened.client,
            child_id,
            "the child settles after its Decision is durable",
            |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
        )
        .await;
        assert!(settled_child.activities.iter().any(|activity| matches!(
            activity,
            Activity::Approval {
                outcome: ApprovalOutcome::Decided,
                decision: Some(Decision::Accept),
                ..
            }
        )));
        session_where(
            &opened.client,
            opened.session_id,
            "the child row settles",
            |snapshot| {
                matches!(
                    the_subagent_row(snapshot),
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                )
            },
        )
        .await;
        let callback = fixture
            .requests()
            .into_iter()
            .find(|message| message["id"] == "child-approval" && message.get("result").is_some())
            .expect("Suru answers the child's native callback");
        assert_eq!(callback["result"], json!({"decision":"accept"}));

        opened.server.shutdown().await.expect("shut down server");
    }
}

/// The Session once `predicate` holds of it, re-read on every published change. `what` names what
/// was being waited for, so a wait that runs out says which one did.
async fn session_where(
    client: &ManagedClient,
    session_id: SessionId,
    what: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Session while it streams");
            if predicate(&snapshot) {
                return snapshot;
            }
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"))
}

/// The Session once the Turn `turn_index` names has settled.
async fn settled_session(
    client: &ManagedClient,
    session_id: SessionId,
    turn_index: usize,
) -> SessionSnapshot {
    session_where(
        client,
        session_id,
        &format!("Codex Turn {turn_index} settles"),
        |snapshot| {
            snapshot
                .turns
                .get(turn_index)
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        },
    )
    .await
}

/// The agent Messages in `snapshot`, in Transcript order.
fn agent_message_contents(snapshot: &SessionSnapshot) -> Vec<&str> {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Agent)
        .map(|message| message.content.as_str())
        .collect()
}

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

/// A collab spawn running in the foreground of the Turn, on the multi-agent wire shape whose
/// items are `collabAgentToolCall`s: the spawn call completes naming the child thread and the
/// prompt it was handed, Suru attaches the child, the child streams its own Message, the parent's
/// wait call reports the child settling, and the parent answers before its Turn completes.
const COLLAB_SPAWN_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"inProgress","senderThreadId":"root-thread","receiverThreadIds":[],"agentsStates":{}}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"prompt":"Map the crate layout","agentsStates":{"child-thread":{"status":"running"}}}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"child-thread","turnId":"child-turn","itemId":"child-message","delta":"Two crates, one workspace."}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":"Two crates, one workspace."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-wait","tool":"wait","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"agentsStates":{"child-thread":{"status":"completed","message":"Two crates, one workspace."}}}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":"The layout is mapped."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      sleep 0.05
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-child"}}'
      ;;
"#;

#[tokio::test]
async fn a_collab_spawn_opens_the_row_and_the_child_session_fed_by_the_childs_own_items() {
    let fixture = ScriptedCodex::new_multiprocess(COLLAB_SPAWN_CODEX);
    let opened = opened_session(&fixture, "codex-subagent-spawn", "Map the crates").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let settled = session_where(
        client,
        session_id,
        "the late attach reply records the child model after both Turns settle",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
                && matches!(
                    the_subagent_row(snapshot),
                    Activity::Subagent { model: Some(_), .. }
                )
        },
    )
    .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        agent_message_contents(&settled),
        ["The layout is mapped."],
        "only the parent's own Message reaches the parent"
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
    assert_eq!(name, "Agent");
    assert_eq!(
        model.as_ref().map(|model| model.as_str()),
        Some("gpt-child")
    );
    assert_eq!(
        description, "Map the crate layout",
        "the row reads the prompt the spawn call carried"
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
        Some("gpt-child"),
        "the attach reply establishes the child's own Model"
    );
    assert_eq!(
        child.session.agent_selection, None,
        "observed child identity does not become mutable Agent Selection"
    );
    assert_eq!(
        agent_message_contents(&child),
        ["Two crates, one workspace."],
        "the child thread's own items fill the child Session"
    );
    let Some(TranscriptItem::Message { message_id }) = child.transcript.first() else {
        panic!(
            "the child's Turn opens with a Message, got {:?}",
            child.transcript
        );
    };
    let opening = child
        .messages
        .iter()
        .find(|message| message.id == *message_id)
        .expect("the opening Message is in the snapshot");
    assert_eq!(
        opening.role,
        MessageRole::Delegation(Delegator {
            session_id,
            name: None,
        }),
        "the spawn call's prompt is a Delegation from the parent's Agent, not a user Message"
    );
    assert_eq!(opening.content, "Map the crate layout");
    assert_eq!(
        opening.turn_id, child.turns[0].id,
        "the spawn's Delegation opens the child's first Turn"
    );

    let resumed = fixture
        .requests()
        .into_iter()
        .find(|request| request.get("method").and_then(Value::as_str) == Some("thread/resume"))
        .expect("Suru attaches the child thread");
    assert_eq!(
        resumed["params"]["threadId"], "child-thread",
        "the attach names the spawned child thread"
    );
    assert_eq!(resumed["params"]["approvalPolicy"], "on-request");
    assert_eq!(resumed["params"]["sandbox"], "workspace-write");
    let started = fixture
        .requests()
        .into_iter()
        .find(|request| request.get("method").and_then(Value::as_str) == Some("thread/start"))
        .expect("Suru starts the parent's thread");
    assert!(
        started["params"]["config"]["mcp_servers.suru"].is_object(),
        "the parent's thread is handed the Broker: {started}"
    );
    assert!(
        resumed["params"].get("config").is_none(),
        "the child's thread inherits the Broker from its parent's rather than being handed it \
         again, which Codex would ignore on a running thread: {resumed}"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// Codex may stream more command output than it retains in the completed item's
/// `aggregatedOutput`. The live stream remains the child's visible output; the
/// capped final value must not tear down the Provider Session that parent and
/// child share.
const CAPPED_CHILD_COMMAND_OUTPUT_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"find the answer","cwd":"/fixture/work","status":"inProgress"}}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"child-thread","turnId":"child-turn","itemId":"child-command","delta":"retained prefix\nstreamed beyond the final cap\n"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"find the answer","cwd":"/fixture/work","status":"completed","aggregatedOutput":"retained prefix\n","exitCode":0}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn capped_final_command_output_does_not_fail_the_parent_or_subagent() {
    let fixture = ScriptedCodex::new_multiprocess(CAPPED_CHILD_COMMAND_OUTPUT_CODEX);
    let opened = opened_session(
        &fixture,
        "codex-subagent-capped-command-output",
        "Delegate a noisy search",
    )
    .await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(parent.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        status,
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);

    let child = settled_session(&opened.client, *child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    let [
        Activity::Command {
            status,
            output,
            exit_status,
            ..
        },
    ] = child.activities.as_slice()
    else {
        panic!(
            "the child keeps its one streamed command, got {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(*exit_status, Some(0));
    assert_eq!(
        output, "retained prefix\nstreamed beyond the final cap\n",
        "the live stream remains authoritative when the final aggregate is capped"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// A spawned agent outliving the Turn, on the multi-agent wire shape whose items are
/// `subAgentActivity`s: the spawn's activity item streams, the parent's Turn completes with the
/// child still working, and — once released — the child's command settles and the parent thread
/// reports the completion under the settled Turn's identity, which Codex documents as expected.
const OUTLIVING_CHILD_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.01; done
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"inProgress"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"completed","aggregatedOutput":"0 vulnerabilities\n","exitCode":0}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"subagent-completed-1","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      ;;
"#;

#[tokio::test]
async fn a_child_completing_after_the_parents_turn_completed_settles_the_row_not_the_session() {
    let fixture = ScriptedCodex::new_multiprocess(OUTLIVING_CHILD_CODEX);
    let opened = opened_session(&fixture, "codex-subagent-outlives", "Audit the crates").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    // The Turn settled at Codex's own boundary while the child thread works on.
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
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the row works on past the Turn's settle"
    );
    assert_eq!(name, "auditor", "the row names the agent by its path");
    let child_id = *child_id;

    fixture.release();
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    let [Activity::Command { status, output, .. }] = child.activities.as_slice() else {
        panic!(
            "the child's execution is the child's one Activity, got {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(output, "0 vulnerabilities\n");

    let after_completion = session_where(
        client,
        session_id,
        "the completion arriving under the settled Turn's identity settles the row",
        |snapshot| {
            matches!(
                the_subagent_row(snapshot),
                Activity::Subagent {
                    status: ActivityStatus::Completed,
                    ..
                }
            )
        },
    )
    .await;
    let Activity::Subagent { duration_ms, .. } = the_subagent_row(&after_completion) else {
        unreachable!()
    };
    assert!(duration_ms.is_some());
    assert_eq!(
        after_completion.turns[0].status,
        TurnStatus::Completed,
        "the late completion is expected, never a Turn failure"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// Codex resumes the parent itself after its child settles, without another Prompt.
#[tokio::test]
async fn a_codex_started_continuation_streams_and_settles_without_a_prompt() {
    let continuation = r#"
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"inProgress","items":[]}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"continuation-turn","item":{"type":"agentMessage","id":"continuation-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"root-thread","turnId":"continuation-turn","itemId":"continuation-message","delta":"The audit is clean."}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"continuation-turn","item":{"type":"agentMessage","id":"continuation-message","text":"The audit is clean."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"completed","items":[]}}}'
"#;
    let script = format!(
        "{}{}  ;;\n",
        OUTLIVING_CHILD_CODEX.trim_end().trim_end_matches(";;"),
        continuation
    );
    let fixture = ScriptedCodex::new_multiprocess(&script);
    let opened = opened_session(&fixture, "codex-native-continuation", "Audit the crates").await;
    let original = settled_session(&opened.client, opened.session_id, 0).await;
    fixture.release();
    let snapshot = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(snapshot.turns.len(), 2);
    assert_eq!(snapshot.turns[0], original.turns[0]);
    let continuation = &snapshot.turns[1];
    assert_eq!(continuation.prompt_id, None);
    assert_eq!(continuation.status, TurnStatus::Completed);
    assert_eq!(agent_message_contents(&snapshot), ["The audit is clean."]);
    assert_eq!(snapshot.messages.last().unwrap().turn_id, continuation.id);
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "turn/start")
            .count(),
        1
    );
    opened.server.shutdown().await.expect("shut down server");
}

const INTERRUPTIBLE_CONTINUATION: &str = r#"
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"inProgress","items":[]}}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":6,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"interrupted","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn a_codex_started_continuation_can_be_interrupted_before_its_first_output() {
    let script = format!(
        "{}{}",
        OUTLIVING_CHILD_CODEX.trim_end().trim_end_matches(";;"),
        INTERRUPTIBLE_CONTINUATION
    );
    let fixture = ScriptedCodex::new_multiprocess(&script);
    let opened = opened_session(&fixture, "codex-continuation-interrupt", "Audit the crates").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    fixture.release();
    session_where(
        &opened.client,
        opened.session_id,
        "the native continuation begins before output",
        |s| {
            s.turns
                .get(1)
                .is_some_and(|turn| turn.status == TurnStatus::Active)
        },
    )
    .await;
    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("interrupt continuation");
    let snapshot = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(snapshot.turns[1].prompt_id, None);
    assert_eq!(snapshot.turns[1].status, TurnStatus::Interrupted);
    let requests = fixture.requests();
    let interrupt = requests
        .iter()
        .find(|r| r["method"] == "turn/interrupt")
        .expect("interrupt reaches Codex");
    assert_eq!(interrupt["params"]["threadId"], "root-thread");
    assert_eq!(interrupt["params"]["turnId"], "continuation-turn");
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn prompts_arriving_during_native_continuation_interruption_each_get_a_turn() {
    for delay_ack in [false, true] {
        let next_prompt = r#"
    *'"method":"turn/start"'*'Continue new work'*)
      printf '%s\n' '{"id":7,"result":{"turn":{"id":"next-turn"}}}'
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"inProgress","items":[]}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"completed","items":[]}}}'
      ;;
"#;
        let second_prompt = next_prompt
            .replace("Continue new work", "Then more work")
            .replace("\"id\":7", "\"id\":8")
            .replace("next-turn", "last-turn");
        let second_prompt = if delay_ack {
            second_prompt.replace("\"id\":8", "\"id\":9")
        } else {
            second_prompt
        };
        let next_prompt = if delay_ack {
            next_prompt
                .lines()
                .filter(|line| !line.contains("turn/completed"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            next_prompt.to_owned()
        };
        let interrupt_next = if delay_ack {
            r#"
    *'"method":"turn/interrupt"'*'"turnId":"next-turn"'*)
      printf '%s\n' '{"id":8,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"completed","items":[]}}}'
      ;;
"#
        } else {
            ""
        };
        let gate_before = if delay_ack {
            "      printf '%s\\n' '{\"id\":6"
        } else {
            "      printf '%s\\n' '{\"method\":\"turn/completed\""
        };
        let interrupted = INTERRUPTIBLE_CONTINUATION.replace(gate_before, &format!(
        "      while [ ! -e \"$CODEX_FIXTURE_RELEASE-2\" ]; do sleep 0.01; done\n{gate_before}"));
        let script = format!(
            "{}{}{}{}{}",
            interrupt_next,
            next_prompt,
            second_prompt,
            OUTLIVING_CHILD_CODEX.trim_end().trim_end_matches(";;"),
            interrupted
        );
        let fixture = ScriptedCodex::new_multiprocess(&script);
        let opened = opened_session(
            &fixture,
            "codex-continuation-new-prompt",
            "Audit the crates",
        )
        .await;
        settled_session(&opened.client, opened.session_id, 0).await;
        fixture.release();
        session_where(
            &opened.client,
            opened.session_id,
            "the native continuation begins",
            |s| {
                s.turns
                    .get(1)
                    .is_some_and(|turn| turn.status == TurnStatus::Active)
            },
        )
        .await;
        let prompt_id = PromptId::new();
        opened
            .client
            .admit_prompt(
                opened.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: prompt_id,
                        text: "Continue new work".into(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit next Prompt");
        fixture.wait_for_method("turn/interrupt").await;
        let second_id = PromptId::new();
        opened
            .client
            .admit_prompt(
                opened.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: second_id,
                        text: "Then more work".into(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit second Prompt while interruption is pending");
        if delay_ack {
            fixture.release_turn(2);
            session_where(
                &opened.client,
                opened.session_id,
                "first replacement Turn begins",
                |s| s.turns.len() == 3,
            )
            .await;
            // The second StartPrompt was queued during the old interrupt RPC and
            // is consumed before this stop, while the replacement Turn is active.
            opened
                .client
                .interrupt_session(opened.session_id)
                .await
                .expect("finish first replacement Turn");
        } else {
            // This idempotent stop acknowledges the preceding Prompt command,
            // while the fixture still holds the native terminal event back.
            opened
                .client
                .interrupt_session(opened.session_id)
                .await
                .expect("stop remains acknowledged");
            fixture.release_turn(2);
        }
        let snapshot = settled_session(&opened.client, opened.session_id, 3).await;
        assert_eq!(snapshot.turns[3].prompt_id, Some(second_id));
        assert_eq!(snapshot.turns[3].status, TurnStatus::Completed);
        assert_eq!(snapshot.turns[2].prompt_id, Some(prompt_id));
        assert_eq!(snapshot.turns[2].status, TurnStatus::Completed);
        assert_eq!(snapshot.turns[1].status, TurnStatus::Interrupted);
        assert!(
            !fixture
                .methods()
                .iter()
                .any(|method| method == "turn/steer")
        );
        opened.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_continuation_selection_failure_preserves_the_error_and_delivers_queued_work() {
    let next_prompt = r#"
    *'"method":"turn/start"'*'Continue new work'*)
      printf '%s\n' '{"id":6,"result":{"turn":{"id":"next-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"completed","items":[]}}}'
      ;;
"#;
    let continuation = r#"
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"inProgress","items":[]}}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE-2" ]; do sleep 0.01; done
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"continuation-turn","status":"failed","error":{"message":"model gpt-fixture is unavailable"},"items":[]}}}'
      ;;
"#;
    let script = format!(
        "{}{}{}",
        next_prompt,
        OUTLIVING_CHILD_CODEX.trim_end().trim_end_matches(";;"),
        continuation
    );
    let fixture = ScriptedCodex::new_multiprocess(&script);
    let opened = opened_session(
        &fixture,
        "codex-continuation-selection-failure",
        "Audit the crates",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;
    fixture.release();
    session_where(
        &opened.client,
        opened.session_id,
        "the native continuation begins",
        |s| s.turns.len() == 2,
    )
    .await;
    opened
        .client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Continue new work".into(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue work after the continuation");
    fixture.release_turn(2);
    let failed = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(failed.turns[1].status, TurnStatus::Failed);
    assert!(
        failed.activities.iter().any(|activity| matches!(activity,
            Activity::Error { text, .. } if text.contains("model gpt-fixture is unavailable")
        )),
        "the native failure is retained: {:?}",
        failed.activities
    );
    let snapshot = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(snapshot.turns[2].status, TurnStatus::Completed);
    assert_eq!(
        snapshot.prompts.len(),
        2,
        "no retry Prompt is invented for the continuation"
    );
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn continuations_keep_selection_and_usage_without_reopening_stale_or_foreign_turns() {
    fn notify(method: &str, params: Value) -> String {
        format!(
            "      printf '%s\\n' '{}'\n",
            json!({"method": method, "params": params})
        )
    }
    fn started(thread: &str, turn: &str) -> String {
        notify(
            "turn/started",
            json!({"threadId": thread, "turn": {"id": turn, "status": "inProgress", "items": []}}),
        )
    }
    fn usage(turn: &str, input: u64) -> String {
        notify(
            "thread/tokenUsage/updated",
            json!({"threadId": "root-thread", "turnId": turn, "tokenUsage": {"total": {"inputTokens": input, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0}}}),
        )
    }
    fn completed(turn: &str) -> String {
        notify(
            "turn/completed",
            json!({"threadId": "root-thread", "turn": {"id": turn, "status": "completed", "items": []}}),
        )
    }
    let response = r#"      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'"#;
    // App-server may announce the requested Turn before its RPC response.
    let initial = OUTLIVING_CHILD_CODEX.replace(response, &format!("{}{}\n{}{}",
        started("root-thread", "root-turn"), response, usage("root-turn", 100),
        notify("thread/settings/updated", json!({"threadId": "root-thread", "threadSettings": {"model": "gpt-effective", "effort": "high"}}))));
    let mut continuation = String::new();
    for thread in ["foreign-thread", "child-thread", "root-thread"] {
        continuation.push_str(&started(thread, "root-turn"));
        continuation.push_str(&notify("item/completed", json!({"threadId": thread, "turnId": "root-turn", "item": {"type": "agentMessage", "id": "stale-message", "text": "Discard this"}})));
    }
    continuation.push_str(&started("root-thread", "continuation-1"));
    continuation.push_str(&notify("item/started", json!({"threadId": "root-thread", "turnId": "continuation-1", "item": {"type": "agentMessage", "id": "message", "text": ""}})));
    continuation.push_str(&started("root-thread", "continuation-1"));
    continuation.push_str(&notify("item/completed", json!({"threadId": "root-thread", "turnId": "continuation-1", "item": {"type": "agentMessage", "id": "message", "text": "Kept"}})));
    continuation.push_str(&usage("continuation-1", 160));
    continuation.push_str(&completed("continuation-1"));
    continuation.push_str(&started("root-thread", "continuation-2"));
    continuation.push_str(&usage("continuation-2", 200));
    continuation.push_str(&completed("continuation-2"));
    let script = format!(
        "{}{}  ;;\n",
        initial.trim_end().trim_end_matches(";;"),
        continuation
    );
    let fixture = ScriptedCodex::new_multiprocess(&script);
    let opened = opened_session(
        &fixture,
        "codex-continuation-correlation",
        "Audit the crates",
    )
    .await;
    let original = settled_session(&opened.client, opened.session_id, 0).await;
    fixture.release();
    let snapshot = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(snapshot.turns.len(), 3);
    assert_eq!(snapshot.turns[0], original.turns[0]);
    assert_eq!(agent_message_contents(&snapshot), ["Kept"]);
    for (turn, expected_usage) in snapshot.turns[1..].iter().zip([60, 40]) {
        assert_eq!(turn.prompt_id, None);
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(
            turn.usage.as_ref().unwrap().fresh_input_tokens,
            Some(expected_usage)
        );
        let selection = &turn.agent.as_ref().unwrap().selection;
        assert_eq!(selection.model.as_str(), "gpt-effective");
        assert_eq!(
            selection,
            &original.turns[0].agent.as_ref().unwrap().selection
        );
    }
    opened.server.shutdown().await.expect("shut down server");
}

/// A spawn inside a spawn: the attached child's own items include a spawn of its own, the
/// grandchild streams under its own thread, and each level settles on its spawner's thread.
const NESTED_SPAWN_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-outer","kind":"started","agentThreadId":"child-thread","agentPath":"/root/planner"}}}'
      ;;
    *'"method":"thread/resume"'*'"threadId":"child-thread"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"subAgentActivity","id":"activity-inner","kind":"started","agentThreadId":"grandchild-thread","agentPath":"/root/planner/scout"}}}'
      ;;
    *'"method":"thread/resume"'*'"threadId":"grandchild-thread"'*)
      printf '%s\n' '{"id":6,"result":{"thread":{"id":"grandchild-thread","parentThreadId":"child-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"grandchild-thread","turnId":"grandchild-turn","item":{"type":"agentMessage","id":"grandchild-message","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"grandchild-thread","turnId":"grandchild-turn","item":{"type":"agentMessage","id":"grandchild-message","text":"Twelve call sites."}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"subAgentActivity","id":"inner-completed","kind":"completed","agentThreadId":"grandchild-thread","agentPath":"/root/planner/scout"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":"Refactor in two steps."}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"outer-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/planner"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":"Here is the plan."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn a_childs_own_spawn_records_the_grandchild_one_level_down() {
    let fixture = ScriptedCodex::new_multiprocess(NESTED_SPAWN_CODEX);
    let opened = opened_session(&fixture, "codex-subagent-nested", "Plan the refactor").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let settled = settled_session(client, session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_message_contents(&settled), ["Here is the plan."]);
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
    assert_eq!(name, "planner");

    let child = settled_session(client, *child_id, 0).await;
    assert_eq!(child.session.parent, Some(session_id));
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_message_contents(&child), ["Refactor in two steps."]);
    let Activity::Subagent {
        status,
        name,
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Completed,
        "the grandchild's settle on the child's thread reaches its row"
    );
    assert_eq!(name, "scout");

    let grandchild = settled_session(client, *grandchild_id, 0).await;
    assert_eq!(
        grandchild.session.parent,
        Some(*child_id),
        "the grandchild is the child's own child, not the parent's"
    );
    assert_eq!(grandchild.turns[0].status, TurnStatus::Completed);
    assert_eq!(agent_message_contents(&grandchild), ["Twelve call sites."]);

    opened.server.shutdown().await.expect("shut down server");
}

/// A collab child in flight while the parent's turn stays open: the spawn's activity item
/// streams, the attached child names its turn by streaming an item of its own, and the
/// conversation waits for the interrupt — which must reach the child's turn before the root's.
const CHILD_IN_FLIGHT_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"inProgress"}}}'
      ;;
    *'"method":"turn/interrupt"'*'"threadId":"child-thread"'*)
      printf '%s\n' '{"id":6,"result":{}}'
      ;;
    *'"method":"turn/interrupt"'*'"threadId":"root-thread"'*)
      printf '%s\n' '{"id":7,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"interrupted","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn interrupting_a_turn_interrupts_the_child_thread_before_the_root() {
    let fixture = ScriptedCodex::new_multiprocess(CHILD_IN_FLIGHT_CODEX);
    let opened = opened_session(&fixture, "codex-interrupt-reaches-children", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let spawned = session_where(client, session_id, "the Subagent's row opens", |snapshot| {
        matches!(snapshot.activities.first(), Some(Activity::Subagent { .. }))
    })
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&spawned)
    else {
        unreachable!()
    };
    let child_id = *child_id;
    // The child's own item must have streamed, because it is what names the
    // turn the stop will address.
    session_where(
        client,
        child_id,
        "the child's command reaches its Session",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;

    client
        .interrupt_session(session_id)
        .await
        .expect("Codex acknowledges the interrupt");
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
        "the stopped Subagent's row settles as stopped"
    );
    assert!(duration_ms.is_some());
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);

    let interrupts = fixture
        .requests()
        .into_iter()
        .filter(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt"))
        .collect::<Vec<_>>();
    let [child_interrupt, root_interrupt] = interrupts.as_slice() else {
        panic!("the child's interrupt and the root's both go out, got {interrupts:?}");
    };
    assert_eq!(
        child_interrupt["params"]["threadId"], "child-thread",
        "the child thread is interrupted before the loop — the established ordering"
    );
    assert_eq!(child_interrupt["params"]["turnId"], "child-turn");
    assert_eq!(root_interrupt["params"]["threadId"], "root-thread");
    assert_eq!(root_interrupt["params"]["turnId"], "root-turn");

    opened.server.shutdown().await.expect("shut down server");
}

/// A collab child outliving the parent's completed turn, its own turn named by the item it
/// streamed, waiting for a stop.
const OUTLIVING_CHILD_TO_STOP_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"inProgress"}}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":6,"result":{}}'
      ;;
"#;

/// Drives the two stops that reach only the child thread: the whole-Session interrupt with no
/// Turn active, and the Picker row's per-Subagent stop. They differ only in the Session the
/// interrupt names, so one scenario proves both.
async fn assert_idle_stop_reaches_only_the_child(name: &'static str, stop_the_child: bool) {
    let fixture = ScriptedCodex::new_multiprocess(OUTLIVING_CHILD_TO_STOP_CODEX);
    let opened = opened_session(&fixture, name, "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let settled = settled_session(client, session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    let child_id = *child_id;
    session_where(
        client,
        child_id,
        "the child's command reaches its Session",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;

    let target = if stop_the_child { child_id } else { session_id };
    client
        .interrupt_session(target)
        .await
        .expect("the stop is acknowledged");

    let stopped = session_where(
        client,
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
    assert_eq!(
        stopped.turns[0].status,
        TurnStatus::Completed,
        "the parent's settled Turn is not re-touched by the stop"
    );
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);

    let interrupts = fixture
        .requests()
        .into_iter()
        .filter(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt"))
        .collect::<Vec<_>>();
    let [interrupt] = interrupts.as_slice() else {
        panic!("exactly the child's interrupt goes out, got {interrupts:?}");
    };
    assert_eq!(
        interrupt["params"]["threadId"], "child-thread",
        "no root turn is running, so nothing but the child is addressed"
    );
    assert_eq!(interrupt["params"]["turnId"], "child-turn");

    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_with_no_turn_active_interrupts_the_outliving_child_thread() {
    assert_idle_stop_reaches_only_the_child("codex-idle-interrupt-children", false).await;
}

#[tokio::test]
async fn stopping_one_subagent_by_its_session_interrupts_its_thread_alone() {
    assert_idle_stop_reaches_only_the_child("codex-stop-one-subagent", true).await;
}

/// A collab child followed but silent: its thread is attached, yet no item of
/// its has streamed, so no turn of its is known to interrupt.
const SILENT_CHILD_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      ;;
"#;

#[tokio::test]
async fn stopping_a_child_that_has_named_no_turn_yet_refuses_rather_than_lying() {
    let fixture = ScriptedCodex::new_multiprocess(SILENT_CHILD_CODEX);
    let opened = opened_session(&fixture, "codex-stop-turnless-child", "Audit").await;
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
        .expect_err("a child with no turn to interrupt cannot be stopped yet");
    assert!(
        refused.to_string().contains("has not begun a turn"),
        "the refusal says why the stop found nothing to address, got: {refused:#}"
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
        "a refused stop settles nothing: the record stays truthful"
    );
    assert!(
        !fixture
            .requests()
            .into_iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
        "no turn is known, so no interrupt goes out"
    );

    opened.server.shutdown().await.expect("shut down server");
}

// Resumes: a delegation that starts another turn on a settled child's thread continues the child
// in its own Session, as a new Turn opened by the delegation, with a row of its own in the Turn
// that delegated it.

/// One scripted line printing the notification `method` with `params`.
fn notify(method: &str, params: Value) -> String {
    format!(
        "      printf '%s\\n' '{}'\n",
        json!({ "method": method, "params": params })
    )
}

/// One scripted line sending Suru the server request `method`, under `id`.
fn server_request(id: &str, method: &str, params: Value) -> String {
    format!(
        "      printf '%s\\n' '{}'\n",
        json!({ "id": id, "method": method, "params": params })
    )
}

/// The scripted lines answering the request just read with `result`, under the id it carried —
/// which depends on how many requests Suru made before it, attaches included.
fn answer(result: Value) -> String {
    format!(
        "      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n      printf '%s\\n' '{{\"id\":'\"$id\"',\"result\":{result}}}'\n"
    )
}

/// One arm of a scripted Codex: `body` runs for each line matching `pattern`.
fn arm(pattern: &str, body: &str) -> String {
    format!("    {pattern})\n{body}      ;;\n")
}

/// The arms every Session opens through: the handshake, the read of the user's configuration —
/// which sets no developer instructions — and the root thread.
fn opening_arms() -> String {
    [
        arm(r#"*'"method":"initialize"'*"#, &answer(json!({}))),
        arm(
            r#"*'"method":"config/read"'*"#,
            &answer(json!({ "config": {}, "origins": {} })),
        ),
        arm(
            r#"*'"method":"thread/start"'*"#,
            &answer(json!({ "thread": { "id": "root-thread" }, "model": "gpt-fixture" })),
        ),
    ]
    .concat()
}

/// The arm answering each attach of the child's thread: the spawn's with `first`, and every later
/// one — a resume attaches the thread again — with `later`.
fn child_attach_arm(first: &str, later: &str) -> String {
    arm(
        r#"*'"method":"thread/resume"'*'"threadId":"child-thread"'*"#,
        &format!(
            "      attaches=$((attaches + 1))\n      if [ \"$attaches\" -eq 1 ]; then\n{first}      else\n{later}      fi\n"
        ),
    )
}

/// The attach reply for the child's thread, naming the Model it runs on.
fn child_attached() -> String {
    answer(json!({
        "thread": { "id": "child-thread", "parentThreadId": "root-thread" },
        "model": "gpt-child",
    }))
}

fn item(stage: &str, thread: &str, turn: &str, item: Value) -> String {
    notify(
        &format!("item/{stage}"),
        json!({ "threadId": thread, "turnId": turn, "item": item }),
    )
}

fn turn_started(thread: &str, turn: &str) -> String {
    notify(
        "turn/started",
        json!({ "threadId": thread, "turn": { "id": turn, "status": "inProgress", "items": [] } }),
    )
}

fn turn_completed(thread: &str, turn: &str) -> String {
    notify(
        "turn/completed",
        json!({ "threadId": thread, "turn": { "id": turn, "status": "completed", "items": [] } }),
    )
}

fn agent_message(thread: &str, turn: &str, id: &str, text: &str) -> String {
    [
        item(
            "started",
            thread,
            turn,
            json!({ "type": "agentMessage", "id": id, "text": "" }),
        ),
        item(
            "completed",
            thread,
            turn,
            json!({ "type": "agentMessage", "id": id, "text": text }),
        ),
    ]
    .concat()
}

/// A collab call the root thread's `turn` completes, naming the child as its one receiver in the
/// lifecycle `state` Codex last knew it in.
fn collab_call(turn: &str, tool: &str, prompt: Option<&str>, state: &str) -> String {
    item(
        "completed",
        "root-thread",
        turn,
        json!({
            "type": "collabAgentToolCall",
            "id": format!("call-{tool}"),
            "tool": tool,
            "status": "completed",
            "senderThreadId": "root-thread",
            "receiverThreadIds": ["child-thread"],
            "prompt": prompt,
            "agentsStates": { "child-thread": { "status": state } },
        }),
    )
}

/// One step of the V2 agent `/root/scout`'s lifecycle, as the root thread's `turn` reports it.
fn agent_activity(turn: &str, kind: &str) -> String {
    item(
        "completed",
        "root-thread",
        turn,
        json!({
            "type": "subAgentActivity",
            "id": format!("activity-{kind}"),
            "kind": kind,
            "agentThreadId": "child-thread",
            "agentPath": "/root/scout",
        }),
    )
}

/// The child thread's running total, read under `turn`, standing at `input` input tokens.
fn child_reading(turn: &str, input: u64) -> String {
    let breakdown = json!({
        "totalTokens": input,
        "inputTokens": input,
        "cachedInputTokens": 0,
        "cacheWriteInputTokens": 0,
        "outputTokens": 0,
        "reasoningOutputTokens": 0,
    });
    notify(
        "thread/tokenUsage/updated",
        json!({
            "threadId": "child-thread",
            "turnId": turn,
            "tokenUsage": { "total": breakdown, "last": breakdown, "modelContextWindow": 272000 },
        }),
    )
}

/// The lines that make the scripted Codex wait for the test to release it.
const AWAIT_RELEASE: &str =
    "      while [ ! -e \"$CODEX_FIXTURE_RELEASE\" ]; do sleep 0.01; done\n";

/// The Subagent rows in `snapshot`, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .collect()
}

/// The Messages in `snapshot`, in order: who each is from, what it says, and the Turn it stands in.
fn messages(snapshot: &SessionSnapshot) -> Vec<(MessageRole, &str, TurnId)> {
    snapshot
        .messages
        .iter()
        .map(|message| {
            (
                message.role.clone(),
                message.content.as_str(),
                message.turn_id,
            )
        })
        .collect()
}

/// What a Delegation from `session_id`'s Agent is drawn as.
fn delegation_from(session_id: SessionId) -> MessageRole {
    MessageRole::Delegation(Delegator {
        session_id,
        name: None,
    })
}

/// The Model a Turn's Agent was observed running, if any.
fn turn_model(turn: &Turn) -> Option<&str> {
    turn.agent
        .as_ref()
        .map(|agent| agent.selection.model.as_str())
}

/// The fresh input tokens a Turn's Usage counts.
fn fresh_input(turn: &Turn) -> Option<u64> {
    turn.usage
        .as_ref()
        .and_then(|usage| usage.fresh_input_tokens)
}

/// How many times Suru attached the child's thread.
fn child_attaches(codex: &ScriptedCodex) -> usize {
    codex
        .requests()
        .into_iter()
        .filter(|request| {
            request["method"] == "thread/resume" && request["params"]["threadId"] == "child-thread"
        })
        .count()
}

/// Asserts the Subagent tree under `session_id` lists the one child `child_id` once, however
/// many rows lead into it.
async fn assert_one_tree_entry(client: &ManagedClient, session_id: SessionId, child_id: SessionId) {
    let mut tree = client.subscribe_subagent_tree(session_id);
    let Some(SubagentTreeEvent::Snapshot(tree)) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree arrives")
    else {
        panic!("the Subagent tree subscription opens with its snapshot");
    };
    let [entry] = tree.subagents.as_slice() else {
        panic!(
            "the resumed child is one entry in the tree, got {:?}",
            tree.subagents
        );
    };
    assert_eq!(entry.session_id, child_id);
    assert_eq!(entry.parent_session_id, session_id);
}

/// A child the spawn's collab call opened, settled at its own turn's end, and sent more work by
/// the parent's `sendInput`. The send's report still carries the child's previous `completed`, as
/// Codex's status can lag the turn a send starts. The turn the send started then streams on the
/// child's own thread, attached again: a command held for Approval, a Message, and a reading of
/// the thread's running total.
fn resumed_by_send_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "Two crates, one workspace.",
        ),
        child_reading("child-turn-1", 100),
        turn_completed("child-thread", "child-turn-1"),
        collab_call("root-turn", "wait", None, "completed"),
        collab_call(
            "root-turn",
            "sendInput",
            Some("List the binaries.\nKeep it short."),
            "completed",
        ),
        turn_started("child-thread", "child-turn-2"),
    ]
    .concat();
    let resumed_stretch = [
        child_attached(),
        item(
            "started",
            "child-thread",
            "child-turn-2",
            json!({
                "type": "commandExecution",
                "id": "child-command",
                "command": "cargo metadata",
                "cwd": "/fixture/work",
                "status": "inProgress",
            }),
        ),
        server_request(
            "child-approval",
            "item/commandExecution/requestApproval",
            json!({
                "threadId": "child-thread",
                "turnId": "child-turn-2",
                "itemId": "child-command",
                "reason": "read the manifest",
                "command": "cargo metadata",
            }),
        ),
    ]
    .concat();
    let decided = [
        item(
            "completed",
            "child-thread",
            "child-turn-2",
            json!({
                "type": "commandExecution",
                "id": "child-command",
                "command": "cargo metadata",
                "cwd": "/fixture/work",
                "status": "completed",
                "aggregatedOutput": "suru\n",
                "exitCode": 0,
            }),
        ),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "One binary: suru.",
        ),
        child_reading("child-turn-2", 160),
        turn_completed("child-thread", "child-turn-2"),
        agent_message(
            "root-thread",
            "root-turn",
            "root-message",
            "Two crates, one binary.",
        ),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                collab_call(
                    "root-turn",
                    "spawnAgent",
                    Some("Map the crate layout"),
                    "running",
                ),
            ]
            .concat(),
        ),
        child_attach_arm(&first_stretch, &resumed_stretch),
        arm(r#"*'"id":"child-approval","result"'*"#, &decided),
    ]
    .concat()
}

#[tokio::test]
async fn a_send_to_a_completed_child_resumes_it_in_its_own_session_as_a_second_turn() {
    let fixture = ScriptedCodex::new_multiprocess(&resumed_by_send_script());
    let opened = opened_session(&fixture, "codex-subagent-resumed-by-send", "Map the crates").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let resuming = session_where(
        client,
        session_id,
        "the resume's row opens beside the spawn's",
        |snapshot| subagent_rows(snapshot).len() == 2,
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = subagent_rows(&resuming)[0]
    else {
        unreachable!()
    };
    let child_id = *child_id;
    let pending = session_where(
        client,
        child_id,
        "the resumed stretch asks for Approval",
        |snapshot| snapshot.pending_approvals.len() == 1,
    )
    .await;
    let [first, second] = pending.turns.as_slice() else {
        panic!(
            "the resume begins a second Turn in the child's Session, got {:?}",
            pending.turns
        );
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(
        second.status,
        TurnStatus::Active,
        "the send's report of the child's previous completion does not settle the Turn it began"
    );
    let Some(Activity::Approval {
        approval, turn_id, ..
    }) = pending
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Approval { .. }))
    else {
        panic!("the child owns its Approval, got {:?}", pending.activities);
    };
    assert_eq!(
        *turn_id, second.id,
        "the resumed stretch's Intervention stands in the Turn the resume began"
    );
    client
        .submit_decision(child_id, approval.id, Decision::Accept)
        .await
        .expect("Codex accepts the resumed child's Decision");

    let parent = session_where(
        client,
        session_id,
        "the parent's Turn settles once the resumed stretch has",
        |snapshot| {
            snapshot.turns[0].status == TurnStatus::Completed
                && subagent_rows(snapshot).iter().all(|row| {
                    matches!(
                        row,
                        Activity::Subagent {
                            status: ActivityStatus::Completed,
                            ..
                        }
                    )
                })
        },
    )
    .await;
    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            description: spawn_description,
            session_id: spawn_child,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
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
        [*spawn_turn, *resume_turn],
        [parent.turns[0].id; 2],
        "the resume row stands in the Turn whose send delegated it"
    );
    assert_eq!(
        (*spawn_child, *resume_child),
        (child_id, child_id),
        "both rows lead into the child's one Session"
    );
    assert_eq!(spawn_description, "Map the crate layout");
    assert_eq!(resume_name, "Agent");
    assert_eq!(
        resume_description, "List the binaries.",
        "the resume row reads the send prompt's first line"
    );
    assert_eq!(
        resume_model.as_ref().map(|model| model.as_str()),
        Some("gpt-child"),
        "the child's Model evidence follows it into the resume's row"
    );
    assert!(resume_duration.is_some());
    assert_eq!(
        agent_message_contents(&parent),
        ["Two crates, one binary."],
        "none of the child's work reaches the parent's Transcript"
    );

    let child = settled_session(client, child_id, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!("the child's Session holds two Turns, got {:?}", child.turns);
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(
        second.status,
        TurnStatus::Completed,
        "the resumed stretch settles at its own turn's end"
    );
    assert_eq!(turn_model(second), Some("gpt-child"));
    assert_eq!(
        messages(&child),
        [
            (
                delegation_from(session_id),
                "Map the crate layout",
                first.id
            ),
            (MessageRole::Agent, "Two crates, one workspace.", first.id),
            (
                delegation_from(session_id),
                "List the binaries.\nKeep it short.",
                second.id,
            ),
            (MessageRole::Agent, "One binary: suru.", second.id),
        ],
        "the second Turn opens with the send's prompt as a Delegation, and the child's Messages \
         land in the Turn they were said in"
    );
    let [
        Activity::Command {
            turn_id: command_turn,
            status: command_status,
            output,
            ..
        },
        Activity::Approval {
            turn_id: approval_turn,
            outcome,
            decision,
            ..
        },
    ] = child.activities.as_slice()
    else {
        panic!(
            "the resumed stretch's command and its Approval are the child's Activities, got {:?}",
            child.activities
        );
    };
    assert_eq!([*command_turn, *approval_turn], [second.id; 2]);
    assert_eq!(*command_status, ActivityStatus::Completed);
    assert_eq!(output, "suru\n");
    assert_eq!(*outcome, ApprovalOutcome::Decided);
    assert_eq!(*decision, Some(Decision::Accept));
    assert_eq!(fresh_input(first), Some(100));
    assert_eq!(
        fresh_input(second),
        Some(60),
        "the resumed Turn meters from where the thread's total stood, not from its start"
    );

    assert_one_tree_entry(client, session_id, child_id).await;
    assert_eq!(
        child_attaches(&fixture),
        2,
        "the send attaches the child's thread again for the turn it starts"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// The child's new turn already started when the send that started it completes — Codex reports
/// the two on different threads, in no promised order — and the send's report still carries the
/// child's previous `completed`. The resumed stretch works on until its own turn ends.
fn stale_report_trailing_the_turn_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "Unit tests pass.",
        ),
        turn_completed("child-thread", "child-turn-1"),
        turn_started("child-thread", "child-turn-2"),
        collab_call(
            "root-turn",
            "sendInput",
            Some("Now the integration tests"),
            "completed",
        ),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "Integration tests pass.",
        ),
        AWAIT_RELEASE.to_owned(),
        turn_completed("child-thread", "child-turn-2"),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                collab_call(
                    "root-turn",
                    "spawnAgent",
                    Some("Run the unit tests"),
                    "running",
                ),
            ]
            .concat(),
        ),
        child_attach_arm(&first_stretch, &child_attached()),
    ]
    .concat()
}

#[tokio::test]
async fn a_stale_completion_in_the_resuming_send_does_not_settle_the_turn_it_began() {
    let fixture = ScriptedCodex::new_multiprocess(&stale_report_trailing_the_turn_script());
    let opened = opened_session(&fixture, "codex-subagent-resume-stale-report", "Test it").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let resuming = session_where(client, session_id, "the resume's row opens", |snapshot| {
        subagent_rows(snapshot).len() == 2
    })
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = subagent_rows(&resuming)[1]
    else {
        unreachable!()
    };
    let child_id = *child_id;
    let working = session_where(
        client,
        child_id,
        "the resumed stretch's Message lands",
        |snapshot| agent_message_contents(snapshot).len() == 2,
    )
    .await;
    assert_eq!(
        working.turns[1].status,
        TurnStatus::Active,
        "the previous completion the send reported leaves the resumed Turn working"
    );
    let parent = client
        .read_session(session_id)
        .await
        .expect("read the parent while the child works");
    assert!(
        matches!(
            subagent_rows(&parent)[1],
            Activity::Subagent {
                status: ActivityStatus::Active,
                ..
            }
        ),
        "and the resume's row with it"
    );

    fixture.release();
    let child = settled_session(client, child_id, 1).await;
    assert_eq!(child.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        messages(&child)[2..],
        [
            (
                delegation_from(session_id),
                "Now the integration tests",
                child.turns[1].id,
            ),
            (
                MessageRole::Agent,
                "Integration tests pass.",
                child.turns[1].id,
            ),
        ]
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// A child closed after its first stretch, reloaded by `resumeAgent` — which starts no turn — and
/// only then sent more work. The script holds after the reload until the test releases it.
fn resumed_after_reload_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "No advisories.",
        ),
        turn_completed("child-thread", "child-turn-1"),
        collab_call("root-turn", "closeAgent", None, "shutdown"),
        collab_call("root-turn", "resumeAgent", None, "pendingInit"),
        agent_message(
            "root-thread",
            "root-turn",
            "root-message-1",
            "Reopened the auditor.",
        ),
        AWAIT_RELEASE.to_owned(),
        collab_call(
            "root-turn",
            "sendInput",
            Some("Audit the lockfile too"),
            "pendingInit",
        ),
    ]
    .concat();
    let resumed_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-2"),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "Lockfile is clean.",
        ),
        turn_completed("child-thread", "child-turn-2"),
        agent_message(
            "root-thread",
            "root-turn",
            "root-message-2",
            "Both are clean.",
        ),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                collab_call(
                    "root-turn",
                    "spawnAgent",
                    Some("Audit the crates"),
                    "running",
                ),
            ]
            .concat(),
        ),
        child_attach_arm(&first_stretch, &resumed_stretch),
    ]
    .concat()
}

#[tokio::test]
async fn a_send_after_resume_agent_resumes_a_closed_child_while_resume_agent_alone_begins_nothing()
{
    let fixture = ScriptedCodex::new_multiprocess(&resumed_after_reload_script());
    let opened = opened_session(&fixture, "codex-subagent-resumed-after-reload", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let reloaded = session_where(
        client,
        session_id,
        "the parent reports the reload",
        |snapshot| agent_message_contents(snapshot) == ["Reopened the auditor."],
    )
    .await;
    let [
        Activity::Subagent {
            session_id: child_id,
            status,
            ..
        },
    ] = subagent_rows(&reloaded)[..]
    else {
        panic!(
            "the reload adds no row of its own, got {:?}",
            reloaded.activities
        );
    };
    let child_id = *child_id;
    assert_eq!(*status, ActivityStatus::Completed);
    let child = client
        .read_session(child_id)
        .await
        .expect("read the reloaded child");
    assert_eq!(
        child.turns.len(),
        1,
        "resumeAgent on its own begins no Turn in the child's Session"
    );
    assert_eq!(child_attaches(&fixture), 1, "nor attaches its thread again");

    fixture.release();
    let parent = settled_session(client, session_id, 0).await;
    let rows = subagent_rows(&parent);
    let [
        Activity::Subagent {
            session_id: spawn_child,
            ..
        },
        Activity::Subagent {
            session_id: resume_child,
            description: resume_description,
            status: resume_status,
            ..
        },
    ] = rows[..]
    else {
        panic!("the send after the reload adds the resume's row, got {rows:?}");
    };
    assert_eq!((*spawn_child, *resume_child), (child_id, child_id));
    assert_eq!(resume_description, "Audit the lockfile too");
    assert_eq!(*resume_status, ActivityStatus::Completed);

    let child = settled_session(client, child_id, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the send begins the child's second Turn, got {:?}",
            child.turns
        );
    };
    assert_eq!(second.status, TurnStatus::Completed);
    assert_eq!(
        messages(&child),
        [
            (delegation_from(session_id), "Audit the crates", first.id),
            (MessageRole::Agent, "No advisories.", first.id),
            (
                delegation_from(session_id),
                "Audit the lockfile too",
                second.id
            ),
            (MessageRole::Agent, "Lockfile is clean.", second.id),
        ]
    );
    assert_one_tree_entry(client, session_id, child_id).await;

    opened.server.shutdown().await.expect("shut down server");
}

/// A V2 agent spawned through `subAgentActivity`, settled, and woken by a `followupTask`: its new
/// turn starts before the parent's thread reports the interaction, and the activity names no
/// prompt. The completion the child's turn end forwards arrives after it.
fn resumed_by_followup_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "Three advisories found.",
        ),
        child_reading("child-turn-1", 100),
        turn_completed("child-thread", "child-turn-1"),
        agent_activity("root-turn", "completed"),
        turn_started("child-thread", "child-turn-2"),
        agent_activity("root-turn", "interacted"),
    ]
    .concat();
    let resumed_stretch = [
        child_attached(),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "All three fixed.",
        ),
        child_reading("child-turn-2", 130),
        turn_completed("child-thread", "child-turn-2"),
        agent_activity("root-turn", "completed"),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                agent_activity("root-turn", "started"),
            ]
            .concat(),
        ),
        child_attach_arm(&first_stretch, &resumed_stretch),
    ]
    .concat()
}

#[tokio::test]
async fn a_followup_task_waking_an_idle_agent_resumes_it_in_its_own_session() {
    let fixture = ScriptedCodex::new_multiprocess(&resumed_by_followup_script());
    let opened = opened_session(&fixture, "codex-subagent-resumed-by-followup", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let parent = settled_session(client, session_id, 0).await;
    let rows = subagent_rows(&parent);
    let [
        Activity::Subagent {
            name: spawn_name,
            session_id: spawn_child,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            name: resume_name,
            description: resume_description,
            session_id: resume_child,
            ..
        },
    ] = rows[..]
    else {
        panic!("the spawn and the followup each stand as a row, got {rows:?}");
    };
    assert_eq!(spawn_child, resume_child, "both rows lead into one Session");
    assert_eq!(*resume_turn, parent.turns[0].id);
    assert_eq!(
        (spawn_name.as_str(), resume_name.as_str()),
        ("scout", "scout"),
        "the resume's row carries the name the agent spawned under"
    );
    assert_eq!(
        resume_description, "",
        "the interaction names no prompt to describe the resume by"
    );
    let child_id = *spawn_child;

    let child = settled_session(client, child_id, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the followup begins the child's second Turn, got {:?}",
            child.turns
        );
    };
    assert_eq!(second.status, TurnStatus::Completed);
    assert_eq!(
        messages(&child),
        [
            (MessageRole::Agent, "Three advisories found.", first.id),
            (MessageRole::Agent, "All three fixed.", second.id),
        ],
        "the followup's work lands in the Turn it began, which opens with no Delegation the \
         wire never carried"
    );
    assert_eq!(fresh_input(first), Some(100));
    assert_eq!(fresh_input(second), Some(30));
    assert_one_tree_entry(client, session_id, child_id).await;

    opened.server.shutdown().await.expect("shut down server");
}

/// A send whose turn starts on the child only after the parent's own turn has completed: Codex
/// reports the child's thread apart from the parent's, and nothing holds the parent's turn open
/// for it. The script holds the resumed turn open until the test releases it.
fn resumed_after_the_parent_settled_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "No advisories.",
        ),
        turn_completed("child-thread", "child-turn-1"),
        collab_call(
            "root-turn",
            "sendInput",
            Some("Audit the lockfile too"),
            "completed",
        ),
        agent_message(
            "root-thread",
            "root-turn",
            "root-message",
            "Sent the auditor back.",
        ),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    let resumed_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-2"),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "Lockfile is clean.",
        ),
        AWAIT_RELEASE.to_owned(),
        turn_completed("child-thread", "child-turn-2"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                collab_call(
                    "root-turn",
                    "spawnAgent",
                    Some("Audit the crates"),
                    "running",
                ),
            ]
            .concat(),
        ),
        child_attach_arm(&first_stretch, &resumed_stretch),
    ]
    .concat()
}

#[tokio::test]
async fn a_resume_whose_turn_starts_after_the_parents_turn_completed_lands_in_a_continuation() {
    let fixture = ScriptedCodex::new_multiprocess(&resumed_after_the_parent_settled_script());
    let opened = opened_session(&fixture, "codex-subagent-resumed-after-parent", "Audit").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let resumed = session_where(
        client,
        session_id,
        "the late resume adds its row",
        |snapshot| subagent_rows(snapshot).len() == 2,
    )
    .await;
    let [prompted, continuation] = resumed.turns.as_slice() else {
        panic!(
            "the resume begins a Turn of its own after the parent's settled one, got {:?}",
            resumed.turns
        );
    };
    assert_eq!(prompted.status, TurnStatus::Completed);
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation, not a Prompt's Turn"
    );
    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            session_id: child_id,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            session_id: resume_child,
            description: resume_description,
            ..
        },
    ] = subagent_rows(&resumed)[..]
    else {
        unreachable!()
    };
    let child_id = *child_id;
    assert_eq!(*spawn_turn, prompted.id);
    assert_eq!(
        *resume_turn, continuation.id,
        "the resume's row stands in the Continuation it began, not the settled Turn that sent it"
    );
    assert_eq!(*resume_child, child_id);
    assert_eq!(resume_description, "Audit the lockfile too");

    let working = session_where(
        client,
        child_id,
        "the resumed stretch's Message lands",
        |snapshot| agent_message_contents(snapshot).len() == 2,
    )
    .await;
    assert_eq!(working.turns[1].status, TurnStatus::Active);
    let parent = client
        .read_session(session_id)
        .await
        .expect("read the parent while the resumed child works");
    assert!(
        parent.working_since().is_some(),
        "the resumed child keeps its parent Working"
    );

    fixture.release();
    let parent = session_where(
        client,
        session_id,
        "the parent stops Working once the resumed stretch settles",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(
        parent.turns[1].status,
        TurnStatus::Completed,
        "Codex runs no parent turn to hold the Continuation open"
    );
    assert!(subagent_rows(&parent).iter().all(|row| matches!(
        row,
        Activity::Subagent {
            status: ActivityStatus::Completed,
            ..
        }
    )));
    let child = settled_session(client, child_id, 1).await;
    assert_eq!(
        messages(&child),
        [
            (
                delegation_from(session_id),
                "Audit the crates",
                child.turns[0].id
            ),
            (MessageRole::Agent, "No advisories.", child.turns[0].id),
            (
                delegation_from(session_id),
                "Audit the lockfile too",
                child.turns[1].id,
            ),
            (MessageRole::Agent, "Lockfile is clean.", child.turns[1].id),
        ]
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// A Session whose first Turn spawns a child that settles, and whose second Turn — after a Server
/// restart relaunched the app-server — sends that child more work. The relaunched app-server
/// never reports the child's spawn: only the send and the child's own thread name it.
fn resumed_after_restart_script() -> String {
    let first_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-1"),
        agent_message(
            "child-thread",
            "child-turn-1",
            "child-message-1",
            "No advisories.",
        ),
        turn_completed("child-thread", "child-turn-1"),
        agent_message(
            "root-thread",
            "root-turn-1",
            "root-message-1",
            "The crates are clean.",
        ),
        turn_completed("root-thread", "root-turn-1"),
    ]
    .concat();
    let resumed_stretch = [
        child_attached(),
        turn_started("child-thread", "child-turn-2"),
        agent_message(
            "child-thread",
            "child-turn-2",
            "child-message-2",
            "Lockfile is clean.",
        ),
        turn_completed("child-thread", "child-turn-2"),
        turn_completed("root-thread", "root-turn-2"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"thread/resume"'*'"threadId":"root-thread"'*"#,
            &answer(json!({ "thread": { "id": "root-thread" }, "model": "gpt-fixture" })),
        ),
        arm(
            r#"*'"method":"turn/start"'*'Send the auditor back'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn-2" } })),
                collab_call(
                    "root-turn-2",
                    "sendInput",
                    Some("Audit the lockfile too"),
                    "completed",
                ),
            ]
            .concat(),
        ),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn-1" } })),
                collab_call("root-turn-1", "spawnAgent", Some("Audit the crates"), "running"),
            ]
            .concat(),
        ),
        // Each app-server the fixture is launched as counts its own attaches, so the one after
        // the restart is told apart by the launch it belongs to.
        arm(
            r#"*'"method":"thread/resume"'*'"threadId":"child-thread"'*"#,
            &format!(
                "      if [ \"$attempt\" -eq 1 ]; then\n{first_stretch}      else\n{resumed_stretch}      fi\n"
            ),
        ),
    ]
    .concat()
}

/// A server hosting the scripted Codex over `state_dir`, and a client connected past its initial
/// state, under the client channel `name`.
async fn hosting(
    codex: &ScriptedCodex,
    name: &'static str,
    state_dir: &std::path::Path,
) -> (server::RunningServer, ManagedClient) {
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir, name).expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    (server, client)
}

#[tokio::test]
async fn a_send_after_a_restart_resumes_the_child_in_the_session_it_spawned_into() {
    let fixture = ScriptedCodex::new_multiprocess(&resumed_after_restart_script());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "codex-subagent-resumed-after-restart";

    let (original, client) = hosting(&fixture, channel, state_dir.path()).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate the audit".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let before = settled_session(&client, session_id, 0).await;
    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
    ] = subagent_rows(&before)[..]
    else {
        panic!(
            "the first Turn spawns the child, got {:?}",
            before.activities
        );
    };
    let child_id = *child_id;
    settled_session(&client, child_id, 0).await;
    drop(client);
    original.shutdown().await.expect("stop the original server");

    let (replacement, client) = hosting(&fixture, channel, state_dir.path()).await;
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Send the auditor back".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt to the reopened Session");
    let parent = settled_session(&client, session_id, 1).await;
    let rows = subagent_rows(&parent);
    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            session_id: spawn_child,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            session_id: resume_child,
            description: resume_description,
            status: resume_status,
            ..
        },
    ] = rows[..]
    else {
        panic!("the spawn and the resume each stand as a row, got {rows:?}");
    };
    assert_eq!(
        [*spawn_turn, *resume_turn],
        [parent.turns[0].id, parent.turns[1].id],
        "the resume row stands in the Turn after the restart that delegated it"
    );
    assert_eq!(
        (*spawn_child, *resume_child),
        (child_id, child_id),
        "the resume leads into the Session the child spawned into before the restart"
    );
    assert_eq!(resume_description, "Audit the lockfile too");
    assert_eq!(*resume_status, ActivityStatus::Completed);

    let child = settled_session(&client, child_id, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the resume begins a second Turn in the child's own Session, got {:?}",
            child.turns
        );
    };
    assert_eq!(second.status, TurnStatus::Completed);
    assert_eq!(
        messages(&child),
        [
            (delegation_from(session_id), "Audit the crates", first.id),
            (MessageRole::Agent, "No advisories.", first.id),
            (
                delegation_from(session_id),
                "Audit the lockfile too",
                second.id
            ),
            (MessageRole::Agent, "Lockfile is clean.", second.id),
        ],
        "the child's thread, found by the identity stored with its Session, continues there"
    );
    assert_eq!(
        turn_model(second),
        Some("gpt-child"),
        "the reattach reports the resumed child's Model"
    );
    assert_one_tree_entry(&client, session_id, child_id).await;

    drop(client);
    replacement.shutdown().await.expect("shut down server");
}

// Steers: a `sendInput` delivered into a child's running turn stands in the Turn it steers, where
// the child drained it, as a Delegation from the Agent that sent it — and nowhere else.

/// Input `thread`'s `turn` drained, as Codex reports it: a `userMessage` item started and
/// completed together, carrying `text`.
fn user_message(thread: &str, turn: &str, id: &str, text: &str) -> String {
    let message = json!({
        "type": "userMessage",
        "id": id,
        "clientId": null,
        "content": [{ "type": "text", "text": text, "text_elements": [] }],
    });
    [
        item("started", thread, turn, message.clone()),
        item("completed", thread, turn, message),
    ]
    .concat()
}

/// A `sendInput` from `sender`'s `turn` to `receiver` handing it `prompt`, at `stage`, with the
/// receiver still running.
fn send_input(stage: &str, sender: &str, turn: &str, receiver: &str, prompt: &str) -> String {
    let status = if stage == "started" {
        "inProgress"
    } else {
        "completed"
    };
    item(
        stage,
        sender,
        turn,
        json!({
            "type": "collabAgentToolCall",
            "id": format!("call-send-from-{sender}"),
            "tool": "sendInput",
            "status": status,
            "senderThreadId": sender,
            "receiverThreadIds": [receiver],
            "prompt": prompt,
            "agentsStates": { receiver: { "status": "running" } },
        }),
    )
}

/// The parent spawns the child, which opens its turn with the spawn's input and says something;
/// the parent's `sendInput` then hands the running turn more, which the child drains before
/// answering it. The child's turn ends there, and the parent's after it.
fn steered_by_send_script() -> String {
    let child_turn = [
        child_attached(),
        turn_started("child-thread", "child-turn"),
        user_message(
            "child-thread",
            "child-turn",
            "child-input-1",
            "Map the crate layout",
        ),
        agent_message(
            "child-thread",
            "child-turn",
            "child-message-1",
            "Two crates so far.",
        ),
        send_input(
            "started",
            "root-thread",
            "root-turn",
            "child-thread",
            "Count the lines too.",
        ),
        send_input(
            "completed",
            "root-thread",
            "root-turn",
            "child-thread",
            "Count the lines too.",
        ),
        user_message(
            "child-thread",
            "child-turn",
            "child-input-2",
            "Count the lines too.",
        ),
        agent_message(
            "child-thread",
            "child-turn",
            "child-message-2",
            "Two crates, 4k lines.",
        ),
        turn_completed("child-thread", "child-turn"),
        collab_call("root-turn", "wait", None, "completed"),
        agent_message("root-thread", "root-turn", "root-message", "Mapped."),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                collab_call(
                    "root-turn",
                    "spawnAgent",
                    Some("Map the crate layout"),
                    "running",
                ),
            ]
            .concat(),
        ),
        arm(
            r#"*'"method":"thread/resume"'*'"threadId":"child-thread"'*"#,
            &child_turn,
        ),
    ]
    .concat()
}

/// The parent's Session once its Turn and every Subagent row in it have settled.
async fn settled_parent(
    client: &ManagedClient,
    session_id: SessionId,
    rows: usize,
) -> SessionSnapshot {
    session_where(
        client,
        session_id,
        "the parent's Turn and its Subagent rows settle",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
                && subagent_rows(snapshot).len() == rows
                && subagent_rows(snapshot).iter().all(|row| {
                    matches!(
                        row,
                        Activity::Subagent {
                            status: ActivityStatus::Completed,
                            ..
                        }
                    )
                })
        },
    )
    .await
}

#[tokio::test]
async fn a_send_into_a_working_childs_turn_steers_that_turn_where_the_child_drained_it() {
    let fixture = ScriptedCodex::new_multiprocess(&steered_by_send_script());
    let opened = opened_session(&fixture, "codex-subagent-steered-by-send", "Map the crates").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let parent = settled_parent(client, session_id, 1).await;
    let [
        Activity::Subagent {
            description,
            session_id: child_id,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the steer adds no row to the parent's Transcript, got {:?}",
            parent.activities
        );
    };
    let child_id = *child_id;
    assert_eq!(
        description, "Map the crate layout",
        "the steer leaves the row reading its spawn's description"
    );
    assert_eq!(
        parent.activities.len(),
        1,
        "the row is all the parent's Transcript carries of the child: {:?}",
        parent.activities
    );
    assert!(
        parent
            .messages
            .iter()
            .all(|message| !matches!(message.role, MessageRole::Delegation(_))),
        "the steer stands nowhere in the parent's Transcript: {:?}",
        parent.messages
    );

    let child = settled_session(client, child_id, 0).await;
    let [turn] = child.turns.as_slice() else {
        panic!(
            "the steer begins no second Turn in the child's Session, got {:?}",
            child.turns
        );
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        messages(&child),
        [
            (delegation_from(session_id), "Map the crate layout", turn.id),
            (MessageRole::Agent, "Two crates so far.", turn.id),
            (delegation_from(session_id), "Count the lines too.", turn.id),
            (MessageRole::Agent, "Two crates, 4k lines.", turn.id),
        ],
        "the Turn opens with the spawn's Delegation once, not again for the item that opened \
         the child's turn, and the steer stands where the child drained it, from the parent"
    );
    assert_eq!(
        child
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Message { .. }))
            .count(),
        4,
        "each Message stands once in the child's Transcript"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// The attach reply for `thread`, spawned by the root thread.
fn attached(thread: &str) -> String {
    answer(json!({
        "thread": { "id": thread, "parentThreadId": "root-thread" },
        "model": "gpt-child",
    }))
}

/// The parent spawns two children; the sibling's `sendInput` hands the first child's running
/// turn more to do, and the first child drains it before the sibling's call has even completed.
fn steered_by_sibling_script() -> String {
    let spawn = |receiver: &str, prompt: &str| {
        item(
            "completed",
            "root-thread",
            "root-turn",
            json!({
                "type": "collabAgentToolCall",
                "id": format!("call-spawn-{receiver}"),
                "tool": "spawnAgent",
                "status": "completed",
                "senderThreadId": "root-thread",
                "receiverThreadIds": [receiver],
                "prompt": prompt,
                "agentsStates": { receiver: { "status": "running" } },
            }),
        )
    };
    let work = [
        attached("sibling-thread"),
        turn_started("child-thread", "child-turn"),
        agent_message(
            "child-thread",
            "child-turn",
            "child-message-1",
            "Two crates so far.",
        ),
        turn_started("sibling-thread", "sibling-turn"),
        send_input(
            "started",
            "sibling-thread",
            "sibling-turn",
            "child-thread",
            "Also count the tests.",
        ),
        user_message(
            "child-thread",
            "child-turn",
            "child-input",
            "Also count the tests.",
        ),
        send_input(
            "completed",
            "sibling-thread",
            "sibling-turn",
            "child-thread",
            "Also count the tests.",
        ),
        agent_message(
            "child-thread",
            "child-turn",
            "child-message-2",
            "Two crates, 30 tests.",
        ),
        turn_completed("child-thread", "child-turn"),
        agent_message(
            "sibling-thread",
            "sibling-turn",
            "sibling-message",
            "The map holds up.",
        ),
        turn_completed("sibling-thread", "sibling-turn"),
        agent_message("root-thread", "root-turn", "root-message", "Reviewed."),
        turn_completed("root-thread", "root-turn"),
    ]
    .concat();
    [
        opening_arms(),
        arm(
            r#"*'"method":"turn/start"'*"#,
            &[
                answer(json!({ "turn": { "id": "root-turn" } })),
                spawn("child-thread", "Map the crate layout"),
                spawn("sibling-thread", "Review the map"),
            ]
            .concat(),
        ),
        arm(
            r#"*'"method":"thread/resume"'*'"threadId":"child-thread"'*"#,
            &attached("child-thread"),
        ),
        arm(
            r#"*'"method":"thread/resume"'*'"threadId":"sibling-thread"'*"#,
            &work,
        ),
    ]
    .concat()
}

#[tokio::test]
async fn a_siblings_send_into_a_working_childs_turn_steers_it_from_the_sibling() {
    let fixture = ScriptedCodex::new_multiprocess(&steered_by_sibling_script());
    let opened = opened_session(
        &fixture,
        "codex-subagent-steered-by-sibling",
        "Map the crates",
    )
    .await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let parent = settled_parent(client, session_id, 2).await;
    let [
        Activity::Subagent {
            description: child_description,
            session_id: child_id,
            ..
        },
        Activity::Subagent {
            description: sibling_description,
            session_id: sibling_id,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the parent's Transcript holds the two spawns' rows alone, got {:?}",
            parent.activities
        );
    };
    let (child_id, sibling_id) = (*child_id, *sibling_id);
    assert_eq!(
        [child_description.as_str(), sibling_description.as_str()],
        ["Map the crate layout", "Review the map"],
        "the steer rewrites neither row"
    );

    let child = settled_session(client, child_id, 0).await;
    let [turn] = child.turns.as_slice() else {
        panic!("the steer begins no second Turn, got {:?}", child.turns);
    };
    let [
        (opening_from, "Map the crate layout", opening_turn),
        (MessageRole::Agent, "Two crates so far.", _),
        (MessageRole::Delegation(steer_from), "Also count the tests.", steer_turn),
        (MessageRole::Agent, "Two crates, 30 tests.", _),
    ] = &messages(&child)[..]
    else {
        panic!(
            "the steer stands in the child's Turn where it drained it, got {:?}",
            messages(&child)
        );
    };
    assert_eq!(*opening_from, delegation_from(session_id));
    assert_eq!([*opening_turn, *steer_turn], [turn.id; 2]);
    assert_eq!(
        steer_from.session_id, sibling_id,
        "the steer names the sibling whose send carried it, not the child's parent"
    );

    let sibling = settled_session(client, sibling_id, 0).await;
    assert!(
        sibling
            .messages
            .iter()
            .all(|message| message.content != "Also count the tests."),
        "the steer stands only in the Transcript of the Subagent it steered: {:?}",
        sibling.messages
    );

    opened.server.shutdown().await.expect("shut down server");
}
