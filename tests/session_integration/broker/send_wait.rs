//! Sending more to brokered Subagents, and waiting on them, through the
//! Broker.
//!
//! `send_to_subagent` delivers a Delegation to a brokered Subagent after its
//! spawn, and what it does is decided by how it is delivered (ADR 0032): to a
//! Subagent whose work has settled it is a resume — a new Turn in the
//! Subagent's own Session, standing as a row of its own in the sending Agent's
//! Transcript that leads into that same Session (ADR 0031) — and to one still
//! working it steers the Turn it works in, adding no row. `wait_subagents`
//! blocks until a Subagent it waits on settles or its bound passes, reporting
//! progress meanwhile so a harness's idle window stays open (ADR 0034, 0035).
//!
//! Each test acts as the MCP client the delegating Agent's harness is, and
//! reads what the Subagents' doubles were handed and what the Transcripts
//! say.

use suru::protocol::{Message, TurnId};
use suru::provider::ProviderResumeState;
use tokio::sync::mpsc;

use super::*;

/// What the Subagents below are sent once they have been spawned.
const FOLLOW_UP: &str = "Now say which of those seams the tests reach.";

/// The answer the Subagents below settle with.
const ANSWER: &str = "Three seams: ProviderRuntime, ProviderSession and the Broker's Tools.";

/// The delegating Agent every test below sends from, as a Delegation names it
/// to the Subagent's Provider.
const CALLER: &str = "the Agent working on \"Plan the work\"";

/// [`FOLLOW_UP`] as the Subagent's Provider receives it from the Agent
/// `delegator` names: one leading line naming that Agent, then what it sent.
fn followed_up_by(delegator: &str) -> String {
    format!("Delegated to you through Suru by {delegator}.\n\n{FOLLOW_UP}")
}

/// Every row `snapshot` holds for the Subagent whose Session is `child`, in
/// the order they were added.
fn rows_for(snapshot: &SessionSnapshot, child: SessionId) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| {
            matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child)
        })
        .collect()
}

/// The Turn a row stands in, how it stands, and how long it says its stretch
/// worked.
fn reading_of(row: &Activity) -> (TurnId, ActivityStatus, Option<u64>) {
    let Activity::Subagent {
        turn_id,
        status,
        duration_ms,
        ..
    } = row
    else {
        unreachable!("a Subagent row")
    };
    (*turn_id, *status, *duration_ms)
}

/// The Messages `snapshot`'s Transcript holds, in the order it holds them.
fn messages_in_order(snapshot: &SessionSnapshot) -> Vec<&Message> {
    snapshot
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Message { message_id } => snapshot
                .messages
                .iter()
                .find(|message| message.id == *message_id),
            _ => None,
        })
        .collect()
}

/// The Delegations `snapshot`'s Transcript holds, in order: each with the
/// Turn it stands in and what it said.
fn delegations(snapshot: &SessionSnapshot) -> Vec<(TurnId, String)> {
    messages_in_order(snapshot)
        .into_iter()
        .filter(|message| matches!(message.role, MessageRole::Delegation(_)))
        .map(|message| (message.turn_id, message.content.clone()))
        .collect()
}

/// How long `turn` worked, from its beginning to its settling.
fn worked(turn: &suru::protocol::Turn) -> Option<u64> {
    turn.settled_at
        .zip(turn.started_at)
        .map(|(settled, started)| settled.0 - started.0)
}

/// The Delegation `caller`'s Agent sent, as it stands in the Subagent's
/// Transcript: a Message from that Agent, apart from a user Message.
fn from_caller(caller: SessionId) -> MessageRole {
    MessageRole::Delegation(Delegator {
        session_id: caller,
        name: None,
    })
}

/// Has the brokered Subagent `child`, working on `provider`, settle its
/// latest stretch having written [`ANSWER`], and takes up the Subagent Report
/// that steers the delegating Agent's working Turn.
async fn settle_child(
    delegating: &mut Delegating,
    child: SessionId,
    provider: &ControlledProviderSession,
) {
    say(&delegating.descriptor, child, provider, ANSWER).await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    steered_by_the_report(&mut delegating.caller_provider, child).await;
}

/// The Turn `provider` is next asked to begin, as `what` it is.
async fn next_turn(
    provider: &mut ControlledProviderSession,
    what: &str,
) -> crate::provider_support::TurnStart {
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .unwrap_or_else(|_| panic!("{what}"))
}

/// A Config Document pinning `document`, in a directory held as long as the
/// test holds it.
fn config_pinning(document: &str) -> tempfile::TempDir {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), document).expect("write Config Document");
    config_dir
}

#[tokio::test]
async fn sending_to_a_settled_brokered_subagent_resumes_it_in_its_own_session_with_a_row_of_its_own()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-resume", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, child_id, &child_provider).await;
    read_until(
        &descriptor,
        delegating.caller,
        "the spawn's row settles with the Subagent's first stretch",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;

    let answer = delegating
        .client
        .send_to_subagent(child_id, FOLLOW_UP)
        .await;
    assert_eq!(
        answer,
        json!({ "session_id": child_id, "delivered": "resumed" }),
        "a Subagent whose work has settled is resumed"
    );
    let resumed = next_turn(
        &mut child_provider,
        "the resume reaches the Subagent's own Provider as a Turn of its own",
    )
    .await;
    assert_eq!(
        resumed.prompt(),
        followed_up_by(CALLER),
        "the Turn's input is the Delegation, naming the Agent that sent it"
    );
    assert_eq!(
        resumed.selection(),
        &codex_selection("high"),
        "on the Subagent's own Agent Selection"
    );
    resumed.succeed();

    let child = read_until(
        &descriptor,
        child_id,
        "the resume's Turn works in the Subagent's own Session",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].agent.is_some(),
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(child.turns[1].status, TurnStatus::Active);
    assert_eq!(
        child.turns[1].prompt_id, None,
        "a Delegation, not a Prompt, begins it"
    );
    let resume_opening = messages_in_order(&child)
        .into_iter()
        .find(|message| message.turn_id == child.turns[1].id)
        .expect("the resume's Turn holds a Message");
    assert_eq!(
        (&resume_opening.role, resume_opening.content.as_str()),
        (&from_caller(delegating.caller), FOLLOW_UP),
        "the resume's Turn opens with the Delegation as a Message from the Agent that sent it"
    );

    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the resume stands as a row of its own",
        |snapshot| rows_for(snapshot, child_id).len() == 2,
    )
    .await;
    let rows = rows_for(&caller, child_id);
    let Activity::Subagent {
        name,
        description,
        brokered,
        ..
    } = rows[1]
    else {
        unreachable!()
    };
    assert_eq!(
        reading_of(rows[1]),
        (caller.turns[0].id, ActivityStatus::Active, None),
        "the resume's row stands working in the Turn whose Agent sent it"
    );
    assert_eq!(caller.turns[0].status, TurnStatus::Active);
    assert_eq!(
        (name.as_str(), description.as_str(), *brokered),
        ("Researcher", FOLLOW_UP, true),
        "naming the Subagent as its spawn did, and what it was asked to do this time"
    );
    assert_eq!(
        reading_of(rows[0]).1,
        ActivityStatus::Completed,
        "while the spawn's row stays as its stretch settled"
    );
    let (tree, _updates) = open_tree(&descriptor, delegating.caller).await;
    assert_eq!(
        tree.subagents
            .iter()
            .filter(|entry| entry.session_id == child_id)
            .count(),
        1,
        "and the Subagent tree still shows the Subagent once, however often it is resumed"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the resume's row settles with its Turn",
        |snapshot| {
            rows_for(snapshot, child_id)
                .last()
                .is_some_and(|row| reading_of(row).1 != ActivityStatus::Active)
        },
    )
    .await;
    let child = read_session(&descriptor, child_id).await;
    let rows = rows_for(&caller, child_id);
    assert_eq!(reading_of(rows[1]).1, ActivityStatus::Completed);
    assert_eq!(
        reading_of(rows[1]).2,
        worked(&child.turns[1]),
        "each row says how long its own stretch worked"
    );
    assert_eq!(reading_of(rows[0]).2, worked(&child.turns[0]));

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn sending_to_a_working_brokered_subagent_steers_its_turn_and_adds_no_row() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-steer", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    say(&descriptor, child_id, &child_provider, "Two seams so far.").await;

    let (answer, ()) = tokio::join!(
        delegating.client.send_to_subagent(child_id, FOLLOW_UP),
        async {
            let steer = timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the Delegation steers the Subagent's working Turn");
            assert_eq!(
                steer.prompt(),
                followed_up_by(CALLER),
                "delivered as the Subagent's Provider takes a steer, naming the Agent that sent it"
            );
            assert!(steer.reports().is_empty());
            steer.succeed();
        }
    );
    assert_eq!(
        answer,
        json!({ "session_id": child_id, "delivered": "steered" }),
        "a Subagent still working is steered"
    );

    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns.len(), 1, "a steer begins no Turn");
    let transcript = messages_in_order(&child);
    assert_eq!(
        transcript
            .iter()
            .map(|message| (&message.role, message.content.as_str()))
            .collect::<Vec<_>>(),
        [
            (&from_caller(delegating.caller), DELEGATION),
            (&MessageRole::Agent, "Two seams so far."),
            (&from_caller(delegating.caller), FOLLOW_UP),
        ],
        "the steer stands as a Delegation from the Agent that sent it, where the Subagent \
         received it: after what it wrote before"
    );
    assert_eq!(transcript[2].turn_id, child.turns[0].id);

    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        rows_for(&caller, child_id).len(),
        1,
        "a steer adds no row to the delegating Transcript"
    );
    assert_eq!(
        row_status(&caller, child_id),
        (ActivityStatus::Active, None)
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_steer_the_subagents_provider_refuses_is_held_and_resumes_the_subagent_once_its_turn_settles()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-steer-refused", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;

    // The Subagent's Turn ends at its Provider as the Delegation arrives, so
    // the Provider no longer takes a steer — and Suru hears of the end only
    // afterwards. Arriving once the work has finished, the Delegation begins
    // a Turn of its own (CONTEXT.md: Delegation).
    let (answer, ()) = tokio::join!(
        delegating.client.send_to_subagent(child_id, FOLLOW_UP),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the Delegation is offered to the Subagent's working Turn")
                .fail("no turn is running");
            let child = read_session(&descriptor, child_id).await;
            assert_eq!(
                delegations(&child),
                [(child.turns[0].id, DELEGATION.to_owned())],
                "a steer never taken stands nowhere in the Turn that refused it"
            );
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
            let resumed = next_turn(
                &mut child_provider,
                "the held Delegation resumes the Subagent once its Turn has settled",
            )
            .await;
            assert_eq!(resumed.prompt(), followed_up_by(CALLER));
            resumed.succeed();
        }
    );
    assert_eq!(
        answer,
        json!({ "session_id": child_id, "delivered": "resumed" }),
        "the call answers with how the message was delivered in the end"
    );
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;

    let child = read_until(
        &descriptor,
        child_id,
        "the resume works in the Subagent's own Session",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    assert_eq!(
        delegations(&child),
        [
            (child.turns[0].id, DELEGATION.to_owned()),
            (child.turns[1].id, FOLLOW_UP.to_owned()),
        ],
        "the Delegation opens the resume's Turn"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        rows_for(&caller, child_id).len(),
        2,
        "and stands as a row of its own"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn delegations_held_behind_a_settling_turn_steer_the_resume_the_first_of_them_begins() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-held-steer", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    let mut second_client = McpClient::handed(&delegating.handoff);
    second_client.initialize().await;
    const SECOND: &str = "And list the tests that reach each seam.";

    // Both reach the Subagent's Turn as it ends, and are held; each steer the
    // double is offered shows its Delegation reached the actor in turn.
    let (first, ()) = tokio::join!(
        delegating.client.send_to_subagent(child_id, FOLLOW_UP),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the first Delegation is offered as a steer")
                .fail("no turn is running");
            let second =
                tokio::spawn(async move { second_client.send_to_subagent(child_id, SECOND).await });
            let steer = timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the second Delegation is offered as a steer");
            assert!(steer.prompt().ends_with(SECOND));
            steer.fail("no turn is running");

            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
            let resumed = next_turn(
                &mut child_provider,
                "the first held Delegation resumes the Subagent",
            )
            .await;
            assert_eq!(resumed.prompt(), followed_up_by(CALLER));
            resumed.succeed();
            let steer = timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the second steers the resume's Turn at once, not after it");
            assert!(steer.prompt().ends_with(SECOND));
            steer.succeed();
            assert_eq!(
                timeout(PROGRESS_DEADLINE, second)
                    .await
                    .expect("the second call answers once its Delegation is delivered")
                    .expect("the second call runs to its answer"),
                json!({ "session_id": child_id, "delivered": "steered" })
            );
        }
    );
    assert_eq!(
        first,
        json!({ "session_id": child_id, "delivered": "resumed" })
    );
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;

    let child = read_session(&descriptor, child_id).await;
    assert_eq!(
        delegations(&child),
        [
            (child.turns[0].id, DELEGATION.to_owned()),
            (child.turns[1].id, FOLLOW_UP.to_owned()),
            (child.turns[1].id, SECOND.to_owned()),
        ],
        "one resume, which the second Delegation steered"
    );
    assert_eq!(
        rows_for(
            &read_session(&descriptor, delegating.caller).await,
            child_id
        )
        .len(),
        2,
        "the steer adds no row of its own"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_stop_reaching_the_subagent_while_a_refused_steer_is_held_withdraws_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-held-stopped", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;

    let (refusal, ()) = tokio::join!(
        delegating.client.refusal(
            "send_to_subagent",
            json!({ "id": child_id, "message": FOLLOW_UP })
        ),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the Delegation is offered as a steer")
                .fail("the turn is busy");
            // The user stops the Subagent while the Delegation is held.
            let (_, ()) = tokio::join!(interrupt(&descriptor, child_id), async {
                timeout(PROGRESS_DEADLINE, child_provider.next_interrupt())
                    .await
                    .expect("the stop reaches the Subagent's Provider")
                    .succeed();
            });
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
                .await;
        }
    );
    assert!(
        refusal.contains("stopped before the message reached it"),
        "the held Delegation is withdrawn, and its Agent told so: {refusal}"
    );
    let child = read_until(
        &descriptor,
        child_id,
        "the Subagent's Turn settles as stopped",
        |snapshot| snapshot.turns[0].status == TurnStatus::Interrupted,
    )
    .await;
    assert_eq!(child.turns.len(), 1, "no resume begins behind the stop");
    assert!(child_provider.try_next_turn().is_none());

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_delegation_held_behind_a_continuation_steers_the_resume_an_earlier_one_began() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-continuation-pair", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, child_id, &child_provider).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::ContinuationStarted {
            selection: codex_selection("high"),
        })
        .await;
    read_until(
        &descriptor,
        child_id,
        "the Subagent works on in a Continuation",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    let mut second_client = McpClient::handed(&delegating.handoff);
    second_client.initialize().await;
    const SECOND: &str = "And list the tests that reach each seam.";

    let (first, ()) = tokio::join!(
        delegating.client.send_to_subagent(child_id, FOLLOW_UP),
        async {
            // The first asks for the Continuation's Provider work to stop;
            // the second is sent while that stop is under way.
            let stop = timeout(PROGRESS_DEADLINE, child_provider.next_interrupt())
                .await
                .expect("the Continuation's Provider work is interrupted for the resume");
            let second =
                tokio::spawn(async move { second_client.send_to_subagent(child_id, SECOND).await });
            stop.succeed();
            // A read through the Server lets the second reach the actor
            // before the Continuation settles, as it would were its Provider
            // slower to say so.
            read_session(&descriptor, child_id).await;
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
                .await;
            let resumed = next_turn(
                &mut child_provider,
                "the first Delegation resumes the Subagent once the Continuation settles",
            )
            .await;
            assert_eq!(resumed.prompt(), followed_up_by(CALLER));
            resumed.succeed();
            let steer = timeout(PROGRESS_DEADLINE, child_provider.next_steer())
                .await
                .expect("the second steers the resume's Turn at once, not after it");
            assert!(steer.prompt().ends_with(SECOND));
            steer.succeed();
            assert_eq!(
                timeout(PROGRESS_DEADLINE, second)
                    .await
                    .expect("the second call answers once its Delegation is delivered")
                    .expect("the second call runs to its answer"),
                json!({ "session_id": child_id, "delivered": "steered" })
            );
        }
    );
    assert_eq!(
        first,
        json!({ "session_id": child_id, "delivered": "resumed" })
    );
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(
        child.turns.len(),
        3,
        "one resume, behind the settled Continuation"
    );
    assert_eq!(
        delegations(&child)
            .into_iter()
            .filter(|(turn_id, _)| *turn_id == child.turns[2].id)
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        [FOLLOW_UP.to_owned(), SECOND.to_owned()],
        "the second Delegation stands in the resume the first began"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn sending_to_a_subagent_working_in_a_continuation_settles_it_and_resumes_the_subagent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-continuation", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, child_id, &child_provider).await;

    // The Subagent's Provider takes up more work of its own accord — woken by
    // something of its own — which is a Continuation of its Session, begun by
    // no Delegation.
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::ContinuationStarted {
            selection: codex_selection("high"),
        })
        .await;
    read_until(
        &descriptor,
        child_id,
        "the Subagent works on in a Continuation",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
    )
    .await;

    let (answer, ()) = tokio::join!(
        delegating.client.send_to_subagent(child_id, FOLLOW_UP),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_interrupt())
                .await
                .expect("the Continuation's Provider work is interrupted before the resume begins")
                .succeed();
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
                .await;
            let resumed = next_turn(
                &mut child_provider,
                "the resume reaches the Subagent once its Continuation has settled",
            )
            .await;
            assert_eq!(resumed.prompt(), followed_up_by(CALLER));
            resumed.succeed();
        }
    );
    assert_eq!(
        answer,
        json!({ "session_id": child_id, "delivered": "resumed" }),
        "a Continuation is settled by the next Delegation rather than steered, as by a Prompt"
    );

    let child = read_until(
        &descriptor,
        child_id,
        "the resume's Turn follows the settled Continuation",
        |snapshot| snapshot.turns.len() == 3,
    )
    .await;
    assert_eq!(child.turns[1].status, TurnStatus::Interrupted);
    assert_eq!(child.turns[2].status, TurnStatus::Active);
    assert_eq!(
        delegations(&child)
            .into_iter()
            .map(|(turn_id, _)| turn_id)
            .collect::<Vec<_>>(),
        [child.turns[0].id, child.turns[2].id],
        "the Delegation opens the resume's Turn, not the Continuation"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    let rows = rows_for(&caller, child_id);
    assert_eq!(rows.len(), 2, "the resume stands as a row of its own");
    assert_eq!(reading_of(rows[1]).1, ActivityStatus::Active);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

/// Asks the Server to interrupt `session_id`, as a client stopping it does.
async fn interrupt(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
        .error_for_status()
        .expect("the interrupt is taken");
}

#[tokio::test]
async fn a_stop_reaching_the_subagent_while_a_delegation_waits_on_its_continuation_withdraws_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-withdrawn", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, child_id, &child_provider).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::ContinuationStarted {
            selection: codex_selection("high"),
        })
        .await;
    read_until(
        &descriptor,
        child_id,
        "the Subagent works on in a Continuation",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;

    let (refusal, ()) = tokio::join!(
        delegating.client.refusal(
            "send_to_subagent",
            json!({ "id": child_id, "message": FOLLOW_UP })
        ),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_interrupt())
                .await
                .expect("the Continuation's Provider work is interrupted for the resume")
                .succeed();
            // The user stops the Subagent before its Continuation settles.
            interrupt(&descriptor, child_id).await;
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
                .await;
        }
    );
    assert!(
        refusal.contains("stopped before the message reached it"),
        "the Delegation waiting behind the stop is withdrawn, and its Agent told so: {refusal}"
    );
    let child = read_until(
        &descriptor,
        child_id,
        "the Continuation settles as stopped",
        |snapshot| snapshot.turns[1].status == TurnStatus::Interrupted,
    )
    .await;
    assert_eq!(child.turns.len(), 2, "no resume begins behind the stop");
    assert!(child_provider.try_next_turn().is_none());
    assert_eq!(
        rows_for(
            &read_session(&descriptor, delegating.caller).await,
            child_id
        )
        .len(),
        1,
        "and no row stands for it"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn after_a_restart_sending_to_a_restored_brokered_subagent_resumes_it_through_its_own_resume_state()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-send-restart";
    let mut delegating = delegating(state_dir.path(), channel, None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    // The Subagent's Provider reports the Resume State that continues its
    // own conversation, which is nothing of its Claude parent's.
    let child_resume = ProviderResumeState::new(json!({ "thread_id": "codex-researcher-thread" }));
    let mut child_provider = next_start(&mut delegating.hosted.codex)
        .await
        .succeed_with_resume(
            AgentIdentity {
                agent: AgentId::new("codex-agent"),
                selection: codex_selection("high"),
            },
            Some(child_resume.clone()),
        );
    next_turn(
        &mut child_provider,
        "the Delegation reaches the Subagent's Provider",
    )
    .await
    .succeed();
    settle_child(&mut delegating, child_id, &child_provider).await;
    read_until(
        &delegating.descriptor,
        delegating.caller,
        "the spawn's row settles",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;
    let caller_id = delegating.caller;
    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
    drop(child_provider);

    let mut restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    // The caller's Agent reaches the Broker again once its next Prompt starts
    // its Provider.
    admit_prompt(
        &descriptor,
        caller_id,
        "Have the Researcher look at the tests.",
    )
    .await;
    let start = next_start(&mut restarted.claude).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker");
    let mut caller_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    next_turn(
        &mut caller_provider,
        "the Prompt reaches the caller's Provider",
    )
    .await
    .succeed();
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;

    assert_eq!(
        client.send_to_subagent(child_id, FOLLOW_UP).await,
        json!({ "session_id": child_id, "delivered": "resumed" }),
        "a restored Subagent is resumed"
    );
    let start = next_start(&mut restarted.codex).await;
    assert_eq!(
        start.resume_state(),
        Some(&child_resume),
        "its own Provider is started again on the Resume State that continues its own \
         conversation"
    );
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    let resumed = next_turn(
        &mut child_provider,
        "the resume reaches the relaunched Provider",
    )
    .await;
    assert_eq!(resumed.prompt(), followed_up_by(CALLER));
    resumed.succeed();

    let child = read_until(
        &descriptor,
        child_id,
        "the resume works in the restored Session",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    assert_eq!(
        delegations(&child),
        [
            (child.turns[0].id, DELEGATION.to_owned()),
            (child.turns[1].id, FOLLOW_UP.to_owned()),
        ],
        "the Session holds the Subagent's whole conversation, the spawn's and the resume's"
    );
    let caller = read_session(&descriptor, caller_id).await;
    let rows = rows_for(&caller, child_id);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        reading_of(rows[1]),
        (
            caller.turns.last().expect("the restarted Prompt's Turn").id,
            ActivityStatus::Active,
            None
        ),
        "the resume's row stands in the Turn the caller's Agent now works in"
    );

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_resume_past_the_concurrency_cap_is_refused_naming_it_and_goes_through_once_there_is_room()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_pinning(r#"{"broker": {"maxConcurrentSubagents": 1}}"#);
    let mut delegating = delegating(
        state_dir.path(),
        "broker-send-capped",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = delegating.descriptor.clone();
    let (settled_id, mut settled_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, settled_id, &settled_provider).await;
    let (working_id, working_provider) = spawn_working_child(&mut delegating).await;

    let refusal = delegating
        .client
        .refusal(
            "send_to_subagent",
            json!({ "id": settled_id, "message": FOLLOW_UP }),
        )
        .await;
    assert_eq!(
        refusal,
        "Suru's Broker lets at most 1 brokered Subagent work at once beneath a top-level Session \
         (`broker.maxConcurrentSubagents`), and 1 is working now, so the Subagent was not \
         resumed. Call wait_subagents to wait for one to settle, or ask the user to raise the \
         Setting."
    );
    assert_eq!(
        read_session(&descriptor, settled_id).await.turns.len(),
        1,
        "nothing of the resume was begun"
    );
    assert_eq!(
        rows_for(
            &read_session(&descriptor, delegating.caller).await,
            settled_id
        )
        .len(),
        1,
        "and no row stands for it"
    );
    assert!(
        settled_provider.try_next_turn().is_none(),
        "nor was the Subagent's Provider asked for anything"
    );

    settle_child(&mut delegating, working_id, &working_provider).await;
    assert_eq!(
        delegating
            .client
            .send_to_subagent(settled_id, FOLLOW_UP)
            .await,
        json!({ "session_id": settled_id, "delivered": "resumed" }),
        "once a Subagent settles there is room, and nothing was queued meanwhile"
    );
    next_turn(&mut settled_provider, "the resume reaches the Subagent")
        .await
        .succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_resume_past_the_cap_is_refused_before_anything_the_subagent_does_is_stopped() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_pinning(r#"{"broker": {"maxConcurrentSubagents": 1}}"#);
    let mut delegating = delegating(
        state_dir.path(),
        "broker-send-capped-continuation",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = delegating.descriptor.clone();
    let (resumable_id, mut resumable_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, resumable_id, &resumable_provider).await;
    let (_working_id, _working_provider) = spawn_working_child(&mut delegating).await;
    // The settled Subagent's Provider takes up work of its own, which a
    // resume of it would have to stop first.
    resumable_provider
        .emit_and_wait_until_observed(ProviderEvent::ContinuationStarted {
            selection: codex_selection("high"),
        })
        .await;
    read_until(
        &descriptor,
        resumable_id,
        "the Subagent works on in a Continuation",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;

    let refusal = delegating
        .client
        .refusal(
            "send_to_subagent",
            json!({ "id": resumable_id, "message": FOLLOW_UP }),
        )
        .await;
    assert!(
        refusal.contains("`broker.maxConcurrentSubagents`")
            && refusal.contains("so the Subagent was not resumed"),
        "the resume is refused naming the cap: {refusal}"
    );
    assert!(
        resumable_provider.try_next_interrupt().is_none(),
        "and nothing the Subagent was doing was stopped for it"
    );
    assert_eq!(
        read_session(&descriptor, resumable_id).await.turns[1].status,
        TurnStatus::Active,
        "its Continuation works on"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_send_naming_no_brokered_subagent_beneath_the_caller_or_carrying_no_message_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-send-refused", None).await;
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    let unknown = SessionId::new();

    for (arguments, says) in [
        (
            json!({ "id": unknown, "message": FOLLOW_UP }),
            format!("Suru holds no Session `{unknown}`"),
        ),
        (
            json!({ "id": delegating.caller, "message": FOLLOW_UP }),
            format!(
                "`{}` is not a Subagent spawned with spawn_subagent by you or by a Subagent \
                 beneath you; send_to_subagent sends only to those.",
                delegating.caller
            ),
        ),
        (
            json!({ "id": child_id, "message": " \n" }),
            "send_to_subagent's `message` is empty".to_owned(),
        ),
        (
            json!({ "id": child_id }),
            "send_to_subagent needs `message`".to_owned(),
        ),
        (
            json!({ "id": child_id, "message": FOLLOW_UP, "urgent": true }),
            "send_to_subagent takes no argument `urgent`".to_owned(),
        ),
        (
            json!({ "message": FOLLOW_UP }),
            "send_to_subagent needs `id`".to_owned(),
        ),
    ] {
        let refusal = delegating
            .client
            .refusal("send_to_subagent", arguments.clone())
            .await;
        assert!(
            refusal.contains(&says),
            "{arguments} is refused saying {says:?}: {refusal}"
        );
    }
    assert!(
        child_provider.try_next_steer().is_none() && child_provider.try_next_turn().is_none(),
        "and nothing reached the Subagent"
    );
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(delegations(&child).len(), 1, "nor stands in its Transcript");

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_answers_once_a_subagent_it_waits_on_settles_with_how_that_subagent_stands() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-wait-settles", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, child_provider) = spawn_working_child(&mut delegating).await;
    let (progress_seen, mut progress) = mpsc::unbounded_channel();

    let (observed, ()) = tokio::join!(
        delegating.client.wait_subagents_observing(
            json!({ "ids": [child_id], "timeout_seconds": 600 }),
            "wait-settles",
            &progress_seen,
        ),
        async {
            timeout(PROGRESS_DEADLINE, progress.recv())
                .await
                .expect("the wait says at once that it is waiting")
                .expect("the wait is still being answered");
            say(&descriptor, child_id, &child_provider, ANSWER).await;
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
        }
    );
    let answer = observed.answer("wait_subagents");
    assert_eq!(answer["timed_out"], json!(false));
    assert_eq!(answer["timeout_seconds"], json!(600));
    assert_eq!(
        answer["settled"],
        json!([delegating.client.read_subagent(child_id).await]),
        "the wait answers with the settled Subagent, as read_subagent reads it"
    );
    assert_eq!(answer["settled"][0]["status"], json!("completed"));
    assert_eq!(answer["settled"][0]["message"], json!(ANSWER));
    // The Report of the settle a wait answered with still reaches the
    // delegating Agent, whose working Turn it steers.
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;

    // A wait on a Subagent that has already settled answers at once.
    assert_eq!(
        delegating
            .client
            .wait_subagents(json!({ "ids": [child_id] }))
            .await["settled"],
        answer["settled"]
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_reports_progress_against_the_calls_token_while_it_waits() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-wait-progress", None).await;
    let (child_id, child_provider) = spawn_working_child(&mut delegating).await;
    let (progress_seen, mut progress) = mpsc::unbounded_channel();

    let (observed, ()) = tokio::join!(
        delegating.client.wait_subagents_observing(
            json!({ "ids": [child_id], "timeout_seconds": 600 }),
            "wait-progress",
            &progress_seen,
        ),
        async {
            for _ in 0..3 {
                timeout(PROGRESS_DEADLINE, progress.recv())
                    .await
                    .expect("the wait reports progress while it waits")
                    .expect("the wait is still being answered");
            }
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
        }
    );
    assert!(
        observed.progress.len() >= 3,
        "the test client observed the progress notifications ahead of the answer: {:?}",
        observed.progress
    );
    for notification in &observed.progress {
        assert_eq!(notification["progressToken"], json!("wait-progress"));
        assert_eq!(
            notification["total"].as_f64(),
            Some(600.0),
            "out of the seconds the wait may take: {notification}"
        );
        assert!(
            notification["message"]
                .as_str()
                .is_some_and(|message| message.contains("Waiting on 1 Subagent")),
            "saying what it waits on: {notification}"
        );
    }
    let progressed = observed
        .progress
        .iter()
        .map(|notification| notification["progress"].as_f64().expect("a progress value"))
        .collect::<Vec<_>>();
    assert!(
        progressed.windows(2).all(|pair| pair[0] < pair[1]),
        "each notification says more progress than the last, as MCP requires: {progressed:?}"
    );
    assert_eq!(
        observed.answer("wait_subagents")["settled"][0]["session_id"],
        json!(child_id)
    );
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_that_sees_nothing_settle_answers_timed_out_at_its_bound() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    // Each of the wait's seconds lasts a millisecond, so its bound of ten is
    // waited out in ten.
    let mut delegating = delegating_timed(
        state_dir.path(),
        "broker-wait-timeout",
        None,
        Duration::from_millis(1),
    )
    .await;
    let (child_id, _child_provider) = spawn_working_child(&mut delegating).await;

    let began = tokio::time::Instant::now();
    let answer = delegating
        .client
        .wait_subagents(json!({ "ids": [child_id], "timeout_seconds": 10 }))
        .await;
    assert_eq!(
        answer,
        json!({ "settled": [], "timed_out": true, "timeout_seconds": 10 }),
        "a wait no Subagent answered says it timed out, so its Agent may call again"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(10),
        "having waited out its bound"
    );
    assert_eq!(
        delegating.client.read_subagent(child_id).await["status"],
        json!("working"),
        "and the Subagent waited on works on"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_timeout_outside_the_bounds_is_clamped_and_the_answer_says_which_it_kept() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating_timed(
        state_dir.path(),
        "broker-wait-clamped",
        None,
        Duration::from_millis(1),
    )
    .await;
    let (child_id, child_provider) = spawn_working_child(&mut delegating).await;

    assert_eq!(
        delegating
            .client
            .wait_subagents(json!({ "ids": [child_id], "timeout_seconds": 1 }))
            .await,
        json!({ "settled": [], "timed_out": true, "timeout_seconds": 10 }),
        "a timeout under ten seconds waits ten"
    );

    settle_child(&mut delegating, child_id, &child_provider).await;
    // A wait on a Subagent that has already settled answers at once, so each
    // of these says what it would have waited without waiting it.
    for (timeout_seconds, kept) in [
        (json!(5000), 600),
        (Value::Null, 60),
        (json!(90.4), 90),
        (json!(-3), 10),
        (json!(600), 600),
    ] {
        let mut arguments = json!({ "ids": [child_id] });
        if !timeout_seconds.is_null() {
            arguments["timeout_seconds"] = timeout_seconds.clone();
        }
        let answer = delegating.client.wait_subagents(arguments).await;
        assert_eq!(
            answer["timeout_seconds"],
            json!(kept),
            "a timeout of {timeout_seconds} is kept as {kept}: {answer}"
        );
        assert_eq!(answer["timed_out"], json!(false));
        assert_eq!(answer["settled"][0]["session_id"], json!(child_id));
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_naming_no_subagent_waits_on_every_working_one_the_caller_spawned() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-wait-every", None).await;
    let descriptor = delegating.descriptor.clone();
    let (earlier_id, earlier_provider) = spawn_working_child(&mut delegating).await;
    settle_child(&mut delegating, earlier_id, &earlier_provider).await;
    let (slow_id, slow_provider) = spawn_working_child(&mut delegating).await;
    let (quick_id, quick_provider) = spawn_working_child(&mut delegating).await;
    let (progress_seen, mut progress) = mpsc::unbounded_channel();

    let (observed, ()) = tokio::join!(
        delegating.client.wait_subagents_observing(
            json!({ "timeout_seconds": 600 }),
            "wait-every",
            &progress_seen
        ),
        async {
            timeout(PROGRESS_DEADLINE, progress.recv())
                .await
                .expect("the wait says at once that it is waiting")
                .expect("the wait is still being answered");
            say(&descriptor, quick_id, &quick_provider, ANSWER).await;
            quick_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
        }
    );
    assert!(
        observed.progress[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Waiting on 2 Subagents")),
        "the wait waits on the two Subagents working, not the one already settled: {:?}",
        observed.progress[0]
    );
    let answer = observed.answer("wait_subagents");
    assert_eq!(
        answer,
        json!({
            "settled": [delegating.client.read_subagent(quick_id).await],
            "timed_out": false,
            "timeout_seconds": 600,
        }),
        "it answers once either settles, with the one that did"
    );
    steered_by_the_report(&mut delegating.caller_provider, quick_id).await;

    settle_child(&mut delegating, slow_id, &slow_provider).await;
    let nothing = delegating.client.wait_subagents(json!({ "ids": [] })).await;
    assert_eq!(
        (
            &nothing["settled"],
            &nothing["timed_out"],
            &nothing["timeout_seconds"]
        ),
        (&json!([]), &json!(false), &json!(60)),
        "with none of its Subagents working, a wait naming none answers at once"
    );
    assert!(
        nothing["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("nothing to wait on")),
        "saying there was nothing to wait on: {nothing}"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_naming_no_brokered_subagent_beneath_the_caller_or_malformed_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-wait-refused", None).await;
    let (child_id, _child_provider) = spawn_working_child(&mut delegating).await;
    let unknown = SessionId::new();

    for (arguments, says) in [
        (
            json!({ "ids": [child_id, unknown] }),
            format!("Suru holds no Session `{unknown}`"),
        ),
        (
            json!({ "ids": [delegating.caller] }),
            format!(
                "`{}` is not a Subagent spawned with spawn_subagent by you or by a Subagent \
                 beneath you; wait_subagents waits only on those.",
                delegating.caller
            ),
        ),
        (
            json!({ "ids": child_id }),
            "wait_subagents' `ids` must be a list of the session_ids".to_owned(),
        ),
        (
            json!({ "ids": ["Researcher"] }),
            "is not a session_id".to_owned(),
        ),
        (
            json!({ "timeout_seconds": "a minute" }),
            "wait_subagents' `timeout_seconds` must be a number of seconds".to_owned(),
        ),
        (
            json!({ "until": "settled" }),
            "wait_subagents takes no argument `until`".to_owned(),
        ),
    ] {
        let refusal = delegating
            .client
            .refusal("wait_subagents", arguments.clone())
            .await;
        assert!(
            refusal.contains(&says),
            "{arguments} is refused saying {says:?}: {refusal}"
        );
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn the_caller_reads_as_waiting_on_its_subagents_only_while_a_wait_waits() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-wait-reading", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, child_provider) = spawn_working_child(&mut delegating).await;
    let reader = reqwest::Client::new();
    let before = read_session(&descriptor, delegating.caller).await;
    assert_eq!(before.waiting_on_subagents, None, "spawning is no waiting");
    let working_since = before.working_since();
    let (progress_seen, mut progress) = mpsc::unbounded_channel();

    let (observed, ()) = tokio::join!(
        delegating.client.wait_subagents_observing(
            json!({ "ids": [child_id], "timeout_seconds": 600 }),
            "wait-reading",
            &progress_seen,
        ),
        async {
            timeout(PROGRESS_DEADLINE, progress.recv())
                .await
                .expect("the wait says at once that it is waiting")
                .expect("the wait is still being answered");
            let waiting = read_session_until(
                &reader,
                &descriptor,
                delegating.caller,
                "the caller waits on its Subagent",
                SessionSnapshot::only_waiting_on_subagents,
            )
            .await;
            assert_eq!(
                waiting.waiting_on_subagents,
                before.turns.last().map(|turn| turn.id),
                "the caller waits in the Turn it spawned from"
            );
            assert_eq!(
                waiting.working_since(),
                working_since,
                "waiting is the caller's Working spent, not a new stretch of it"
            );
            say(&descriptor, child_id, &child_provider, ANSWER).await;
            child_provider
                .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
                .await;
        }
    );
    assert_eq!(observed.answer("wait_subagents")["timed_out"], json!(false));
    read_session_until(
        &reader,
        &descriptor,
        delegating.caller,
        "a wait answered with a settle leaves the caller waiting on nothing",
        |snapshot| snapshot.waiting_on_subagents.is_none(),
    )
    .await;
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_timing_out_or_refused_leaves_the_caller_waiting_on_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    // Each of the wait's seconds lasts a millisecond, so its bound of ten is
    // waited out in ten.
    let mut delegating = delegating_timed(
        state_dir.path(),
        "broker-wait-reading-timeout",
        None,
        Duration::from_millis(1),
    )
    .await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, _child_provider) = spawn_working_child(&mut delegating).await;
    let reader = reqwest::Client::new();

    assert_eq!(
        delegating
            .client
            .wait_subagents(json!({ "ids": [child_id], "timeout_seconds": 10 }))
            .await["timed_out"],
        json!(true)
    );
    read_session_until(
        &reader,
        &descriptor,
        delegating.caller,
        "a wait that timed out leaves the caller waiting on nothing",
        |snapshot| snapshot.waiting_on_subagents.is_none(),
    )
    .await;

    let refusal = delegating
        .client
        .refusal(
            "wait_subagents",
            json!({ "ids": [child_id, SessionId::new()] }),
        )
        .await;
    assert!(refusal.contains("Suru holds no Session"), "{refusal}");
    assert_eq!(
        read_session(&descriptor, delegating.caller)
            .await
            .waiting_on_subagents,
        None,
        "a refused wait never waited"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_wait_whose_client_has_gone_leaves_the_caller_waiting_on_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    // A wait of 600 seconds lasts a minute here, far past the deadline for
    // the caller to stop reading as waiting.
    let mut delegating = delegating(state_dir.path(), "broker-wait-reading-gone", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, _child_provider) = spawn_working_child(&mut delegating).await;
    let reader = reqwest::Client::new();

    // A harness that gives up on a wait closes its connection; the wait learns
    // so at the next progress it writes, and stops waiting with it.
    let mut leaving = McpClient::handed(&delegating.handoff);
    leaving.initialize().await;
    let (progress_seen, mut progress) = mpsc::unbounded_channel();
    let abandoned = tokio::spawn(async move {
        leaving
            .wait_subagents_observing(
                json!({ "ids": [child_id], "timeout_seconds": 600 }),
                "wait-abandoned",
                &progress_seen,
            )
            .await
    });
    timeout(PROGRESS_DEADLINE, progress.recv())
        .await
        .expect("the wait says at once that it is waiting")
        .expect("the wait is still being answered");
    read_session_until(
        &reader,
        &descriptor,
        delegating.caller,
        "the caller waits on its Subagent",
        SessionSnapshot::only_waiting_on_subagents,
    )
    .await;
    abandoned.abort();
    let _ = abandoned.await;
    read_session_until(
        &reader,
        &descriptor,
        delegating.caller,
        "a wait whose client has gone leaves the caller waiting on nothing",
        |snapshot| snapshot.waiting_on_subagents.is_none(),
    )
    .await;

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
