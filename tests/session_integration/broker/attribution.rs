//! Attributing a Broker call to the native Subagent that made it. A native
//! Subagent shares its parent's Provider connection, and with it the Broker
//! token its parent's start carried; Codex names the thread that made each
//! call in its `_meta.threadId`, and a native Subagent's thread id is the
//! identity Suru knows it by. A call naming a native Subagent that connection
//! spawned is that Subagent's, so every Tool acts for it: a spawn is recorded
//! beneath it, and it reads and stops only what lies beneath it. A call naming
//! nothing that connection knows stays the token's Session's (ADR 0034, ADR
//! 0035, `docs/validation/0408-subagent-mcp-attribution.md`).
//!
//! A brokered Subagent a native Subagent delegated to reports to that native
//! Subagent when it settles, through its Provider's route to it: the Report is
//! the native Subagent's own input, which wakes a settled one into a
//! Continuation of its own Session and stands in no Transcript.
//!
//! The delegating Session runs on the double hosted as Codex, whose native
//! Subagents are known by their thread ids, and each brokered Subagent on the
//! double hosted as Claude.

use suru::provider::{ProviderEventAttribution, SubagentReportOutcome};

use super::reports::{ANSWER, held_in, report_of};
use super::stops::acknowledge_interrupt;
use super::*;
use crate::provider_support::SubagentDelivery;

/// The thread the Codex Session's own Agent runs on, which Codex names in
/// every call's `_meta.sessionId` — its native Subagents' calls included.
const ROOT_THREAD: &str = "019a0e25-60f7-root-thread";

/// The thread a native Subagent of the Codex Session runs on, and so the
/// identity Suru knows it by.
const NATIVE_THREAD: &str = "019a0e25-8213-child-thread";

/// A Codex Session whose first Turn is working, on a Server hosting it beside
/// Claude and an unavailable Copilot, and the MCP client its own Agent is.
async fn codex_delegating(state_dir: &Path, channel: &str) -> Delegating {
    let mut hosted = host_providers(state_dir, channel, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (caller, handoff, caller_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut client = McpClient::handed(&handoff).with_call_meta(json!({
        "threadId": ROOT_THREAD,
        "sessionId": ROOT_THREAD,
    }));
    client.initialize().await;
    Delegating {
        hosted,
        descriptor,
        caller,
        caller_provider,
        handoff,
        client,
    }
}

/// The Session of the native Subagent `provider` spawns for `parent`'s Agent,
/// known by `thread`, once the row leading into it has opened.
async fn spawn_native(
    descriptor: &RuntimeDescriptor,
    parent: SessionId,
    provider: &ControlledProviderSession,
    thread: &str,
) -> SessionId {
    provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new(thread),
            name: "Explore".to_owned(),
            description: "Map the seams".to_owned(),
            delegation: Some("Map every seam.".to_owned()),
        })
        .await;
    let parent = read_until(
        descriptor,
        parent,
        "the native Subagent's row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Some(Activity::Subagent { session_id, .. }) = parent
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        unreachable!()
    };
    *session_id
}

/// The MCP client the native Subagent known by `thread` is: its parent's
/// token, with its own thread named in every call's metadata beside the
/// parent's, as Codex names them.
async fn native_client(handoff: &BrokerHandoff, thread: &str) -> McpClient {
    let mut client = McpClient::handed(handoff).with_call_meta(json!({
        "threadId": thread,
        "sessionId": ROOT_THREAD,
    }));
    client.initialize().await;
    client
}

/// `spawn_subagent`'s arguments for a Subagent named Researcher on Claude's
/// Haiku.
fn haiku_researcher() -> Value {
    researcher("claude", "haiku", json!({}))
}

/// The Agent Selection a Claude Subagent on Haiku runs under.
fn haiku() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("haiku"),
        options: Vec::new(),
    }
}

/// Whether `snapshot` holds a row leading into `child`.
fn has_row_for(snapshot: &SessionSnapshot, child: SessionId) -> bool {
    snapshot.activities.iter().any(|activity| {
        matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child)
    })
}

/// What `delegating`'s Provider is next asked to hand one of its native
/// Subagents itself.
async fn next_delivery(delegating: &mut Delegating) -> SubagentDelivery {
    timeout(
        PROGRESS_DEADLINE,
        delegating.caller_provider.next_subagent_delivery(),
    )
    .await
    .expect("a Report reaches the native Subagent through its Provider")
}

/// A native Subagent of `delegating`'s Session whose stretch of work has
/// settled, and a brokered Subagent its thread spawned after that settle,
/// whose own Provider has taken up its Delegation: the native Subagent's
/// Session, the brokered Subagent's, its Provider, and the Delegation that
/// Provider received.
async fn spawned_after_the_native_subagent_settled(
    delegating: &mut Delegating,
) -> (SessionId, SessionId, ControlledProviderSession, String) {
    let descriptor = delegating.descriptor.clone();
    let native_id = spawn_native(
        &descriptor,
        delegating.caller,
        &delegating.caller_provider,
        NATIVE_THREAD,
    )
    .await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: ProviderSubagentId::new(NATIVE_THREAD),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    read_until(
        &descriptor,
        native_id,
        "the native Subagent's stretch of work settles",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;

    // A call from its thread arrives all the same — one racing that settle,
    // say — while the token's Session still works in its first Turn.
    let mut native = native_client(&delegating.handoff, NATIVE_THREAD).await;
    let child_id = native.spawn_subagent(haiku_researcher()).await;
    let (child_provider, delivered) = run_child(&mut delegating.hosted.claude, haiku()).await;
    (native_id, child_id, child_provider, delivered)
}

#[tokio::test]
async fn a_spawn_naming_a_working_native_subagents_thread_is_recorded_beneath_it_in_the_turn_it_works_in()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-working").await;
    let descriptor = delegating.descriptor.clone();
    let (tree, mut updates) = open_tree(&descriptor, delegating.caller).await;
    let mut revision = tree.revision;
    let native_id = spawn_native(
        &descriptor,
        delegating.caller,
        &delegating.caller_provider,
        NATIVE_THREAD,
    )
    .await;
    let mut native = native_client(&delegating.handoff, NATIVE_THREAD).await;

    let child_id = native.spawn_subagent(haiku_researcher()).await;

    let child = read_session(&descriptor, child_id).await;
    assert_eq!(
        child.session.parent,
        Some(native_id),
        "the brokered Subagent is the native Subagent's child, though the call carried its \
         parent's token"
    );
    let native_session = read_session(&descriptor, native_id).await;
    assert_eq!(
        native_session.turns.len(),
        1,
        "no Turn is begun to hold the row"
    );
    let Activity::Subagent {
        turn_id,
        status,
        brokered,
        ..
    } = row_for(&native_session, child_id)
    else {
        unreachable!()
    };
    assert_eq!(
        (*turn_id, *status, *brokered),
        (native_session.turns[0].id, ActivityStatus::Active, true),
        "its row stands in the Turn the native Subagent works in"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert!(
        !has_row_for(&caller, child_id),
        "and nowhere in the token's Session"
    );
    assert_eq!(caller.turns.len(), 1);

    let Some(TranscriptItem::Message { message_id }) = child.transcript.first() else {
        panic!(
            "the child opens with its Delegation: {:?}",
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
            session_id: native_id,
            name: Some("Explore".to_owned()),
        }),
        "the Delegation stands as the native Subagent's, named as its row names it"
    );
    let (child_provider, delivered) = run_child(&mut delegating.hosted.claude, haiku()).await;
    assert_eq!(
        delivered,
        delegated_by("the Subagent \"Explore\""),
        "and reaches the brokered Subagent's Provider naming the native Subagent"
    );

    let spawned = changes_until(&mut updates, &mut revision, |change| {
        matches!(
            change,
            SubagentTreeChange::SubagentSpawned { entry } if entry.session_id == child_id
        )
    })
    .await;
    let Some(SubagentTreeChange::SubagentSpawned { entry }) = spawned.last() else {
        unreachable!()
    };
    assert_eq!(
        (
            entry.parent_session_id,
            entry.spawn_order,
            entry.name.as_str()
        ),
        (native_id, 0, "Researcher"),
        "the tree shows the brokered Subagent beneath the native Subagent"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        native_id,
        "the row in the native Subagent's Session settles with the brokered Subagent's Turn",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    let (status, duration_ms) = row_status(&settled, child_id);
    assert_eq!(status, ActivityStatus::Completed);
    assert!(duration_ms.is_some());

    // Settling, it reports to the native Subagent that delegated to it, whose
    // Provider steers the stretch it still works in.
    let delivery = next_delivery(&mut delegating).await;
    assert_eq!(delivery.subagent(), NATIVE_THREAD);
    assert_eq!(
        delivery.reports(),
        [report_of(
            &settled,
            child_id,
            SubagentReportOutcome::Completed,
            None
        )]
    );
    delivery.succeed();
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: ProviderSubagentId::new(NATIVE_THREAD),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let native_session = read_until(
        &descriptor,
        native_id,
        "the native Subagent's stretch settles",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(
        (
            native_session.turns.len(),
            held_in(&native_session, native_session.turns[0].id)
        ),
        (1, 2),
        "the Report steered the stretch it worked in, beginning no Turn, and stands nowhere: \
         that stretch holds its Delegation and the brokered Subagent's row alone"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_spawn_naming_a_settled_native_subagents_thread_opens_a_continuation_of_its_session_to_hold_the_row()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-settled").await;
    let descriptor = delegating.descriptor.clone();
    let (native_id, child_id, child_provider, delivered) =
        spawned_after_the_native_subagent_settled(&mut delegating).await;

    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.session.parent, Some(native_id));
    let native_session = read_session(&descriptor, native_id).await;
    assert_eq!(
        native_session.turns.len(),
        2,
        "a Turn is begun in the native Subagent's Session to hold the row: {:?}",
        native_session.turns
    );
    let continuation = &native_session.turns[1];
    assert_eq!(
        (continuation.prompt_id, continuation.status),
        (None, TurnStatus::Completed),
        "a Continuation, settled as it is begun"
    );
    assert!(continuation.started_at.is_some() && continuation.settled_at.is_some());
    let Activity::Subagent {
        turn_id, status, ..
    } = row_for(&native_session, child_id)
    else {
        unreachable!()
    };
    assert_eq!(
        (*turn_id, *status),
        (continuation.id, ActivityStatus::Active),
        "the row stands in that Continuation, working on past its settle"
    );
    assert!(
        native_session.working_since().is_some(),
        "the native Subagent's Session reads as Working through its brokered Subagent"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert!(
        !has_row_for(&caller, child_id),
        "and the token's working Turn holds no row for it"
    );
    assert_eq!(caller.turns.len(), 1);

    assert_eq!(delivered, delegated_by("the Subagent \"Explore\""));
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        native_id,
        "the row settles with the brokered Subagent's Turn, though the Turn holding it has settled",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;
    // What its Report does is the next tests' to read.
    let delivery = next_delivery(&mut delegating).await;
    assert_eq!(delivery.subagent(), NATIVE_THREAD);
    delivery.succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_report_to_a_settled_native_subagent_is_its_own_input_and_wakes_a_continuation_of_its_session()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-report").await;
    let descriptor = delegating.descriptor.clone();
    let (native_id, child_id, child_provider, _) =
        spawned_after_the_native_subagent_settled(&mut delegating).await;

    say(&descriptor, child_id, &child_provider, ANSWER).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let delivery = next_delivery(&mut delegating).await;
    let settled = read_until(
        &descriptor,
        native_id,
        "the brokered Subagent's row settles",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;
    assert_eq!(
        delivery.subagent(),
        NATIVE_THREAD,
        "the Report is handed to the native Subagent that delegated, by the identity its \
         Provider knows it by"
    );
    assert_eq!(
        delivery.reports(),
        [report_of(
            &settled,
            child_id,
            SubagentReportOutcome::Completed,
            Some(ANSWER)
        )],
        "carrying the outcome and the brokered Subagent's final Message"
    );
    assert_eq!(
        settled.turns.len(),
        2,
        "nothing is woken before its Provider takes the Report"
    );
    delivery.succeed();

    let woken = read_until(
        &descriptor,
        native_id,
        "the Report wakes the native Subagent",
        |snapshot| snapshot.turns.len() == 3,
    )
    .await;
    let continuation = &woken.turns[2];
    assert_eq!(
        (continuation.prompt_id, continuation.status),
        (None, TurnStatus::Active),
        "into a Continuation of its own Session, begun by neither a Prompt nor a Delegation"
    );
    assert_eq!(
        held_in(&woken, continuation.id),
        0,
        "in which no Transcript row stands for the Report"
    );
    assert!(woken.working_since().is_some());
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        (caller.turns.len(), caller.activities.len()),
        (1, 1),
        "and the token's Session gains nothing: its one Turn holds the native Subagent's row \
         alone"
    );

    // What the native Subagent then does lands in that Continuation, and its
    // Provider's next settle of it settles the Continuation.
    let native = ProviderEventAttribution::Subagent(ProviderSubagentId::new(NATIVE_THREAD));
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "The Researcher found the seams.".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ] {
        delegating
            .caller_provider
            .emit_attributed_and_wait_until_observed(native.clone(), event)
            .await;
    }
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: ProviderSubagentId::new(NATIVE_THREAD),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let idle = read_until(
        &descriptor,
        native_id,
        "the Continuation settles with the native Subagent's stretch",
        |snapshot| snapshot.turns[2].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(
        idle.messages
            .iter()
            .filter(|message| message.turn_id == idle.turns[2].id)
            .map(|message| (message.role.clone(), message.content.as_str()))
            .collect::<Vec<_>>(),
        [(MessageRole::Agent, "The Researcher found the seams.")]
    );
    assert_eq!(idle.working_since(), None);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_report_the_native_subagents_provider_refuses_wakes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-refused").await;
    let descriptor = delegating.descriptor.clone();
    let (native_id, _child_id, child_provider, _) =
        spawned_after_the_native_subagent_settled(&mut delegating).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    next_delivery(&mut delegating)
        .await
        .fail("This Provider offers no route to deliver input to one of its Subagents");
    // Its parent works on; once its Provider settles the parent's Turn, the
    // refusal has long been read.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let native = read_until(
        &descriptor,
        native_id,
        "nothing beneath the native Subagent works any more",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(
        native.turns.len(),
        2,
        "no Continuation is woken for a Report its Provider would not take: {:?}",
        native.turns
    );
    assert!(
        native
            .turns
            .iter()
            .all(|turn| turn.status == TurnStatus::Completed)
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_spawn_naming_no_native_subagent_of_the_tokens_own_connection_stays_the_tokens_sessions()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-fallback").await;
    let descriptor = delegating.descriptor.clone();
    let workspace = delegating.hosted.workspace.path().to_owned();
    let native_id = spawn_native(
        &descriptor,
        delegating.caller,
        &delegating.caller_provider,
        NATIVE_THREAD,
    )
    .await;

    // Another tree, whose own connection knows a native Subagent of its own.
    let (stranger, _stranger_handoff, stranger_provider) = start_session(
        &descriptor,
        &mut delegating.hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let strangers_native =
        spawn_native(&descriptor, stranger, &stranger_provider, "stranger-thread").await;

    for (meta, says) in [
        (
            json!({ "threadId": "019a0e25-never-spawned" }),
            "a thread no Session is known by",
        ),
        (
            json!({ "threadId": "stranger-thread" }),
            "a native Subagent another tree's connection spawned",
        ),
        (
            json!({ "threadId": ROOT_THREAD, "sessionId": NATIVE_THREAD }),
            "the calling thread, whatever `sessionId` names",
        ),
        (json!({ "threadId": 7 }), "a thread id that is no string"),
        (
            json!({ "claudecode/toolUseId": "toolu_01X4Ei", "progressToken": 3 }),
            "no thread, as Claude's and Copilot's calls name none",
        ),
    ] {
        let mut client = McpClient::handed(&delegating.handoff).with_call_meta(meta.clone());
        client.initialize().await;
        let child_id = client.spawn_subagent(haiku_researcher()).await;

        let child = read_session(&descriptor, child_id).await;
        assert_eq!(
            child.session.parent,
            Some(delegating.caller),
            "a call naming {says} is the token's Session's: {meta}"
        );
        let caller = read_session(&descriptor, delegating.caller).await;
        let Activity::Subagent { turn_id, .. } = row_for(&caller, child_id) else {
            unreachable!()
        };
        assert_eq!(
            *turn_id, caller.turns[0].id,
            "its row stands in the Turn the token's Session works in: {meta}"
        );
        for elsewhere in [native_id, strangers_native, stranger] {
            assert!(
                !has_row_for(&read_session(&descriptor, elsewhere).await, child_id),
                "and nowhere else: {meta}"
            );
        }
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_native_subagent_reads_and_stops_only_the_brokered_subagents_beneath_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = codex_delegating(state_dir.path(), "broker-attribution-scope").await;
    let descriptor = delegating.descriptor.clone();
    let native_id = spawn_native(
        &descriptor,
        delegating.caller,
        &delegating.caller_provider,
        NATIVE_THREAD,
    )
    .await;
    let mut native = native_client(&delegating.handoff, NATIVE_THREAD).await;
    let natives = native.spawn_subagent(haiku_researcher()).await;
    let (mut natives_provider, _) = run_child(&mut delegating.hosted.claude, haiku()).await;
    let parents = delegating.client.spawn_subagent(haiku_researcher()).await;
    let (mut parents_provider, _) = run_child(&mut delegating.hosted.claude, haiku()).await;
    assert_eq!(
        read_session(&descriptor, natives).await.session.parent,
        Some(native_id)
    );
    assert_eq!(
        read_session(&descriptor, parents).await.session.parent,
        Some(delegating.caller)
    );

    assert_eq!(
        native.read_subagent(natives).await["status"],
        json!("working"),
        "the native Subagent reads the brokered Subagent it spawned"
    );
    let refusal = native
        .refusal("read_subagent", json!({ "id": parents }))
        .await;
    assert!(
        refusal.contains(
            "is not a Subagent spawned with spawn_subagent by you or by a Subagent beneath you"
        ),
        "but not one its parent spawned beside it: {refusal}"
    );
    let refusal = native
        .refusal("stop_subagent", json!({ "id": parents }))
        .await;
    assert!(
        refusal.contains("names no Subagent spawned through the Broker beneath you"),
        "nor stops one: {refusal}"
    );
    assert_eq!(
        delegating.client.read_subagent(natives).await["status"],
        json!("working"),
        "while the token's Session reads what lies beneath its native Subagent, as it lies \
         beneath the token's Session too"
    );

    let (answer, ()) = tokio::join!(
        native.stop_subagent(natives),
        acknowledge_interrupt(
            &mut natives_provider,
            "the native Subagent's brokered Subagent's"
        ),
    );
    assert_eq!(
        answer,
        json!({ "session_id": natives, "stopped": true }),
        "the native Subagent stops the brokered Subagent it spawned"
    );
    assert!(
        parents_provider.try_next_interrupt().is_none()
            && delegating.caller_provider.try_next_interrupt().is_none(),
        "and nothing else"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
