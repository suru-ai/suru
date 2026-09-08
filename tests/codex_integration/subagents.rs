//! Codex collab threads as Subagents: a spawn item on the parent thread opens the inline row and
//! a child Session, Suru attaches the child thread so its own items stream into that Session, a
//! child completion arriving after the parent's turn completed settles the row rather than
//! erroring, and a child's own spawns recurse one level down.

use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::{Value, json};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, CreateSessionRequest, InitialPrompt,
        MessageRole, PromptDelivery, PromptId, SessionId, SessionSnapshot, TurnStatus,
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
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
      printf '%s\n' '{"id":5,"result":{}}'
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
      printf '%s\n' '{"id":6,"result":{"turn":{"id":"next-turn"}}}'
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"inProgress","items":[]}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"completed","items":[]}}}'
      ;;
"#;
        let second_prompt = next_prompt
            .replace("Continue new work", "Then more work")
            .replace("\"id\":6", "\"id\":7")
            .replace("next-turn", "last-turn");
        let second_prompt = if delay_ack {
            second_prompt.replace("\"id\":7", "\"id\":8")
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
      printf '%s\n' '{"id":7,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"next-turn","status":"completed","items":[]}}}'
      ;;
"#
        } else {
            ""
        };
        let gate_before = if delay_ack {
            "      printf '%s\\n' '{\"id\":5"
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
      printf '%s\n' '{"id":5,"result":{"turn":{"id":"next-turn"}}}'
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
    let response = r#"      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'"#;
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
