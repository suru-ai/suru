//! Codex collab threads as Subagents: a spawn item on the parent thread opens the inline row and
//! a child Session, Suru attaches the child thread so its own items stream into that Session, a
//! child completion arriving after the parent's turn completed settles the row rather than
//! erroring, and a child's own spawns recurse one level down.

use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::Value;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, ActivityStatus, CreateSessionRequest, InitialPrompt, MessageRole, PromptId,
        SessionId, SessionSnapshot, TurnStatus, Workspace,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: prompt.to_owned(),
                skill_invocations: Vec::new(),
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
    timeout(Duration::from_secs(10), async {
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"inProgress","senderThreadId":"root-thread","receiverThreadIds":[],"agentsStates":{}}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"prompt":"Map the crate layout","agentsStates":{"child-thread":{"status":"running"}}}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"child-thread","turnId":"child-turn","itemId":"child-message","delta":"Two crates, one workspace."}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"agentMessage","id":"child-message","text":"Two crates, one workspace."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-wait","tool":"wait","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"agentsStates":{"child-thread":{"status":"completed","message":"Two crates, one workspace."}}}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"agentMessage","id":"root-message","text":"The layout is mapped."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn a_collab_spawn_opens_the_row_and_the_child_session_fed_by_the_childs_own_items() {
    let fixture = ScriptedCodex::new_multiprocess(COLLAB_SPAWN_CODEX);
    let opened = opened_session(&fixture, "codex-subagent-spawn", "Map the crates").await;
    let session_id = opened.session_id;
    let client = &opened.client;

    let settled = settled_session(client, session_id, 0).await;
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
        ..
    } = the_subagent_row(&settled)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Agent");
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
        agent_message_contents(&child),
        ["Two crates, one workspace."],
        "the child thread's own items fill the child Session"
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
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

/// A spawn inside a spawn: the attached child's own items include a spawn of its own, the
/// grandchild streams under its own thread, and each level settles on its spawner's thread.
const NESTED_SPAWN_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-outer","kind":"started","agentThreadId":"child-thread","agentPath":"/root/planner"}}}'
      ;;
    *'"method":"thread/resume"'*'"threadId":"child-thread"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"subAgentActivity","id":"activity-inner","kind":"started","agentThreadId":"grandchild-thread","agentPath":"/root/planner/scout"}}}'
      ;;
    *'"method":"thread/resume"'*'"threadId":"grandchild-thread"'*)
      printf '%s\n' '{"id":5,"result":{"thread":{"id":"grandchild-thread","parentThreadId":"child-thread"},"model":"gpt-fixture"}}'
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"inProgress"}}}'
      ;;
    *'"method":"turn/interrupt"'*'"threadId":"child-thread"'*)
      printf '%s\n' '{"id":5,"result":{}}'
      ;;
    *'"method":"turn/interrupt"'*'"threadId":"root-thread"'*)
      printf '%s\n' '{"id":6,"result":{}}'
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"cargo audit","cwd":"/fixture/work","status":"inProgress"}}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":5,"result":{}}'
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
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/auditor"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-fixture"}}'
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
