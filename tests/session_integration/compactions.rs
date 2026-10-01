//! Compactions at the provider-neutral seam: a Provider compacting a
//! conversation's context records a Compaction in the Turn it fell in — or in
//! a Continuation it begins when no Turn is active — in the Session whose
//! context it compacted, settling as the Provider reports or with its Turn,
//! and kept as history like any other Activity. A count the Provider left out
//! is read from the Session's own Context Fill instead: `before` as last read
//! before the Compaction began, `after` as first read once it Settled. A
//! Compaction the user asks for begins a Turn of its own (ADR 0041), which
//! only an idle top-level Session on a Provider that compacts on request
//! takes, and which Settles as its Compaction does. The summary a completed
//! one left is stored under Suru's cap, which flags what it cut as Truncation.
//! That Turn takes no steer: a Prompt sent while it runs is held to begin the
//! next Turn once the Compaction completes, and withdrawn if it fails or is
//! interrupted.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    WorkingTurn, compact, controlled_selection, interrupt, read_session, read_session_until,
    the_subagent_row, working_turn,
};
use axum::http::StatusCode;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity, Approval, ApprovalId,
        ApprovalSubject, CompactionTrigger, ContextFill, InitialPrompt, MessageRole, Prompt,
        PromptDelivery, PromptId, PromptStatus, RuntimeDescriptor, SessionChange, SessionError,
        SessionErrorCode, SessionId, SessionListItem, SessionSnapshot, TranscriptItem, TurnStatus,
        Usage,
    },
    provider::{ContextFillReport, ProviderEvent, ProviderEventAttribution, ProviderSubagentId},
    server::{self, ServerConfig},
};
use tokio::time::timeout;

fn completed(before_tokens: Option<u64>, after_tokens: Option<u64>) -> ProviderEvent {
    ProviderEvent::CompactionCompleted {
        before_tokens,
        after_tokens,
        summary: None,
    }
}

/// A Compaction completing with no counts, leaving the Agent `summary`.
fn summarised(summary: &str) -> ProviderEvent {
    ProviderEvent::CompactionCompleted {
        before_tokens: None,
        after_tokens: None,
        summary: Some(summary.to_owned()),
    }
}

fn failed(error: &str) -> ProviderEvent {
    ProviderEvent::CompactionFailed {
        error: Some(error.to_owned()),
    }
}

/// The Provider reading the Session's Context Fill at `tokens`, as the
/// `sequence`th reading of the Turn it is routed to.
fn reading(sequence: u64, tokens: u64) -> ProviderEvent {
    ProviderEvent::ContextFill {
        report: ContextFillReport {
            turn_id: None,
            sequence,
            fill: ContextFill {
                occupied_tokens: tokens,
                capacity_tokens: Some(272_000),
            },
        },
    }
}

/// Each Compaction in `snapshot`, in Transcript order, as its status and the
/// Context Fill before and after it.
fn measured(snapshot: &SessionSnapshot) -> Vec<(ActivityStatus, Option<u64>, Option<u64>)> {
    compactions(snapshot)
        .into_iter()
        .map(|compaction| match compaction {
            Activity::Compaction {
                status,
                before_tokens,
                after_tokens,
                ..
            } => (*status, *before_tokens, *after_tokens),
            _ => unreachable!(),
        })
        .collect()
}

async fn admit_prompt(descriptor: &RuntimeDescriptor, session_id: SessionId, text: &str) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt")
        .error_for_status()
        .expect("Prompt admission succeeds");
}

async fn session_where(
    fixture: &WorkingTurn,
    session_id: SessionId,
    described: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        session_id,
        described,
        predicate,
    )
    .await
}

fn compactions(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Compaction { .. }))
        .collect()
}

/// Each Compaction's summary in `snapshot`, in Transcript order, beside
/// whether Suru's cap cut it.
fn summaries(snapshot: &SessionSnapshot) -> Vec<(Option<&str>, bool)> {
    compactions(snapshot)
        .into_iter()
        .map(|compaction| match compaction {
            Activity::Compaction {
                summary,
                summary_truncated,
                ..
            } => (summary.as_deref(), *summary_truncated),
            _ => unreachable!(),
        })
        .collect()
}

/// The status of each Compaction in `snapshot`, in Transcript order.
fn compaction_statuses(snapshot: &SessionSnapshot) -> Vec<ActivityStatus> {
    compactions(snapshot)
        .into_iter()
        .filter_map(Activity::status)
        .collect()
}

fn turn_settled(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot
        .turns
        .get(turn)
        .is_some_and(|turn| turn.status != TurnStatus::Active)
}

#[tokio::test]
async fn an_automatic_compaction_stands_active_in_its_turn_then_settles_with_the_providers_counts()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-automatic-test").await;
    let session_id = fixture.session_id;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    // A Provider restates that it is still compacting while it summarises;
    // that is the same occasion, not another.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let compacting = session_where(&fixture, session_id, "the Compaction opens", |snapshot| {
        !compactions(snapshot).is_empty()
    })
    .await;
    assert_eq!(
        compactions(&compacting),
        vec![&Activity::Compaction {
            id: compactions(&compacting)[0].id(),
            turn_id: compacting.turns[0].id,
            status: ActivityStatus::Active,
            trigger: CompactionTrigger::Automatic,
            before_tokens: None,
            after_tokens: None,
            error: None,
            summary: None,
            summary_truncated: false,
        }],
        "one Compaction stands Active in the Turn it fell in"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(completed(Some(182_000), Some(31_000)))
        .await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        compactions(&settled),
        vec![&Activity::Compaction {
            id: compactions(&compacting)[0].id(),
            turn_id: settled.turns[0].id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Automatic,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
            summary: None,
            summary_truncated: false,
        }],
        "the Compaction settles where it stood, with the Context Fill the Provider reported"
    );
    assert_eq!(
        settled.turns.len(),
        1,
        "an automatic Compaction begins no Turn"
    );
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_compaction_and_its_retry_stand_as_two_and_leave_the_turn_to_the_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-failed-test").await;
    let session_id = fixture.session_id;

    fixture
        .provider_session
        .emit(ProviderEvent::CompactionStarted);
    fixture
        .provider_session
        .emit(failed("Conversation too long to summarise"));
    // The retry's completion arrives with no start before it, which records
    // a Compaction settled from the moment it stands.
    fixture
        .provider_session
        .emit(completed(Some(190_000), None));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    let [first, retry] = compactions(&settled)[..] else {
        panic!(
            "each attempt is its own Compaction: {:?}",
            settled.activities
        );
    };
    let Activity::Compaction { status, error, .. } = first else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("Conversation too long to summarise"),
        "a failed Compaction keeps the Provider's account of why"
    );
    let Activity::Compaction {
        status,
        before_tokens,
        after_tokens,
        error,
        ..
    } = retry
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(190_000), None),
        "a side the Provider did not report stays absent"
    );
    assert_eq!(*error, None);
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "a failed automatic Compaction leaves its Turn to Settle as the Provider says"
    );
    assert!(
        !settled
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Error { .. })),
        "the failure is the Compaction's alone: {:?}",
        settled.activities
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_still_active_when_its_turn_settles_settles_with_it() {
    // One that failed keeps the reading it began from; one that was stopped
    // left the context as it was, and carries no Context Fill at all.
    for (settle, turn_status, compaction_status, before_tokens) in [
        (
            ProviderEvent::TurnInterrupted,
            TurnStatus::Interrupted,
            ActivityStatus::Interrupted,
            None,
        ),
        (
            ProviderEvent::TurnFailed {
                message: "the CLI exited".to_owned(),
            },
            TurnStatus::Failed,
            ActivityStatus::Failed,
            Some(182_000),
        ),
        (
            ProviderEvent::TurnCompleted,
            TurnStatus::Completed,
            ActivityStatus::Failed,
            Some(182_000),
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let fixture = working_turn(state_dir.path(), "compaction-turn-settle-test").await;
        fixture.provider_session.emit(reading(1, 182_000));
        fixture
            .provider_session
            .emit(ProviderEvent::CompactionStarted);
        fixture.provider_session.emit(settle);
        let settled = session_where(
            &fixture,
            fixture.session_id,
            "the Turn settles",
            |snapshot| turn_settled(snapshot, 0),
        )
        .await;
        assert_eq!(settled.turns[0].status, turn_status);
        assert_eq!(
            measured(&settled),
            vec![(compaction_status, before_tokens, None)],
            "a Compaction still Active when its Turn Settles {turn_status:?} Settles with it"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_compaction_failing_after_suru_interrupted_its_turn_settles_interrupted() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "compaction-interrupt-test").await;
    let session_id = fixture.session_id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;

    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), session_id),
        async {
            timeout(PROGRESS_DEADLINE, fixture.provider_session.next_interrupt())
                .await
                .expect("the interrupt reaches the Provider")
                .succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // A Provider reports the Compaction it was asked to cancel as a failure,
    // ahead of the boundary that ends the Turn.
    fixture
        .provider_session
        .emit_and_wait_until_observed(failed("Request was aborted."))
        .await;
    let cancelled = read_session(fixture.server.descriptor(), session_id).await;
    let [Activity::Compaction { status, error, .. }] = compactions(&cancelled)[..] else {
        panic!("one Compaction is recorded: {:?}", cancelled.activities);
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "the failure Suru asked for is the Compaction stopped, not gone wrong"
    );
    assert_eq!(*error, None, "a stop carries no failure to explain");
    assert_eq!(cancelled.turns[0].status, TurnStatus::Active);

    fixture
        .provider_session
        .emit(ProviderEvent::TurnInterrupted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Interrupted]);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subagents_compaction_stands_in_its_own_session_and_never_its_parents() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-subagent-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = session_where(
        &fixture,
        fixture.session_id,
        "the Subagent row opens",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = *the_subagent_row(&parent)
    else {
        unreachable!()
    };

    for event in [
        ProviderEvent::CompactionStarted,
        completed(Some(90_000), Some(12_000)),
    ] {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }

    let child = session_where(&fixture, child_id, "the child compacts", |snapshot| {
        compaction_statuses(snapshot) == [ActivityStatus::Completed]
    })
    .await;
    let [
        Activity::Compaction {
            turn_id,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&child)[..]
    else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, child.turns[0].id,
        "the Subagent's Compaction stands in the Turn its stretch works in"
    );
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(90_000), Some(12_000))
    );
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        compactions(&parent).is_empty(),
        "the parent's Transcript carries nothing of its Subagent's Compaction: {:?}",
        parent.activities
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_reported_while_no_turn_is_active_begins_a_continuation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-continuation-test").await;
    let session_id = fixture.session_id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    session_where(&fixture, session_id, "the first Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    // Nothing is owed the Session — no Subagent works on, no Watch woke it —
    // and still the Provider compacting is work a Turn must hold.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let compacting = session_where(&fixture, session_id, "a Continuation opens", |snapshot| {
        snapshot.turns.len() == 2
    })
    .await;
    let continuation = &compacting.turns[1];
    assert!(
        continuation.is_continuation(),
        "the Compaction begins a Continuation: {continuation:?}"
    );
    assert_eq!(continuation.status, TurnStatus::Active);
    let [
        Activity::Compaction {
            turn_id, status, ..
        },
    ] = compactions(&compacting)[..]
    else {
        panic!("the Compaction stands in the Continuation: {compacting:?}");
    };
    assert_eq!(*turn_id, continuation.id);
    assert_eq!(*status, ActivityStatus::Active);

    fixture
        .provider_session
        .emit(completed(Some(182_000), Some(31_000)));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(
        &fixture,
        session_id,
        "the Continuation settles",
        |snapshot| turn_settled(snapshot, 1),
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Completed]);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_with_no_counts_reads_them_from_the_context_fill_either_side_of_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-fallback-test").await;
    let session_id = fixture.session_id;

    for event in [reading(1, 182_000), ProviderEvent::CompactionStarted] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let compacting = session_where(&fixture, session_id, "the Compaction opens", |snapshot| {
        !compactions(snapshot).is_empty()
    })
    .await;
    assert_eq!(
        measured(&compacting),
        [(ActivityStatus::Active, Some(182_000), None)],
        "the Compaction begins from the Context Fill last read before it"
    );

    // What the Provider reads while it summarises — its own summarising call,
    // the context it rebuilt — describes no side of the Compaction.
    for event in [
        reading(2, 190_000),
        reading(3, 30_000),
        completed(None, None),
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let compacted = read_session(fixture.server.descriptor(), session_id).await;
    assert_eq!(
        measured(&compacted),
        [(ActivityStatus::Completed, Some(182_000), None)],
        "nothing has been read since the Compaction settled, so its after is still unknown"
    );

    for event in [reading(4, 35_000), reading(5, 40_000)] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [(ActivityStatus::Completed, Some(182_000), Some(35_000))],
        "its after is the first reading once it settled, and no later one"
    );
    assert_eq!(
        settled
            .session
            .context_fill
            .map(|fill| fill.occupied_tokens),
        Some(40_000),
        "the Session's Context Fill reads on as it always did"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_side_no_reading_exists_for_stays_absent_until_one_is_read_even_after_the_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-fallback-absent-test").await;
    let session_id = fixture.session_id;

    // Nothing was read before the Compaction began, and nothing after it
    // before its Turn settled.
    for event in [
        ProviderEvent::CompactionStarted,
        completed(None, None),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [(ActivityStatus::Completed, None, None)],
        "a side with no reading is left absent rather than guessed"
    );

    // The Provider reads the context once more after the Turn settled.
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(1, 35_000))
        .await;
    let measured_after = session_where(
        &fixture,
        session_id,
        "the Compaction is measured after its Turn settled",
        |snapshot| measured(snapshot)[0].2.is_some(),
    )
    .await;
    assert_eq!(
        measured(&measured_after),
        [(ActivityStatus::Completed, None, Some(35_000))],
        "the first reading after it settled is its after, whenever it comes"
    );
    assert_eq!(
        measured_after.turns.len(),
        1,
        "a reading begins no Continuation"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_count_the_provider_reported_is_never_replaced_by_a_reading() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-fallback-reported-test").await;
    let session_id = fixture.session_id;

    for event in [
        reading(1, 100_000),
        ProviderEvent::CompactionStarted,
        // The Provider measured before itself, and nothing after.
        completed(Some(182_000), None),
        reading(2, 35_000),
        ProviderEvent::CompactionStarted,
        // Now it measured after, and nothing before.
        completed(None, Some(31_000)),
        reading(3, 40_000),
        // And here both.
        ProviderEvent::CompactionStarted,
        completed(Some(190_000), Some(20_000)),
        reading(4, 25_000),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [
            (ActivityStatus::Completed, Some(182_000), Some(35_000)),
            (ActivityStatus::Completed, Some(35_000), Some(31_000)),
            (ActivityStatus::Completed, Some(190_000), Some(20_000)),
        ],
        "the Provider's own count stands on each side it reported, and a reading fills only \
         the side it left out"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn consecutive_compactions_each_take_the_readings_either_side_of_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-fallback-sequence-test").await;
    let session_id = fixture.session_id;

    for event in [
        reading(1, 182_000),
        // A failed attempt freed nothing, so what was read before it still
        // stands before its retry.
        ProviderEvent::CompactionStarted,
        failed("Conversation too long to summarise"),
        ProviderEvent::CompactionStarted,
        completed(None, None),
        // Another Compaction before anything was read after that one: the
        // last reading before it began is still the one before the first, and
        // the first reading after both settled is after each of them.
        ProviderEvent::CompactionStarted,
        completed(None, None),
        reading(2, 20_000),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [
            (ActivityStatus::Failed, Some(182_000), None),
            (ActivityStatus::Completed, Some(182_000), Some(20_000)),
            (ActivityStatus::Completed, Some(182_000), Some(20_000)),
        ],
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_reading_taken_while_a_compaction_runs_is_an_earlier_ones_after_but_not_its_own() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-fallback-overlap-test").await;
    let session_id = fixture.session_id;

    for event in [
        reading(1, 182_000),
        ProviderEvent::CompactionStarted,
        completed(None, None),
        ProviderEvent::CompactionStarted,
        // Read while the second Compaction summarises: the first one's after,
        // and no side of the second's.
        reading(2, 30_000),
        completed(None, None),
        reading(3, 22_000),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [
            (ActivityStatus::Completed, Some(182_000), Some(30_000)),
            (ActivityStatus::Completed, Some(182_000), Some(22_000)),
        ],
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_still_awaiting_its_after_takes_the_first_reading_after_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-fallback-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    for event in [
        reading(1, 182_000),
        ProviderEvent::CompactionStarted,
        completed(None, None),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        measured(&settled),
        [(ActivityStatus::Completed, Some(182_000), None)]
    );
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    // Nothing was read after the Compaction before the stop, so the first
    // reading the next process takes is still the first after it settled.
    let (runtime, mut provider) = crate::provider_support::ControlledProvider::new();
    let restarted = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            runtime,
        ),
    )
    .await
    .expect("the server restarts in time")
    .expect("respawn server");
    admit_prompt(restarted.descriptor(), session_id, "Carry on").await;
    let mut provider_session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-subagent", "high", "fast"),
        });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the next Turn reaches the Provider")
        .succeed();
    provider_session
        .emit_and_wait_until_observed(reading(1, 35_000))
        .await;
    let measured_after = read_session_until(
        &reqwest::Client::new(),
        restarted.descriptor(),
        session_id,
        "the Compaction is measured after the restart",
        |snapshot| measured(snapshot)[0].2.is_some(),
    )
    .await;
    assert_eq!(
        measured(&measured_after),
        [(ActivityStatus::Completed, Some(182_000), Some(35_000))]
    );
    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn compactions_are_stored_with_the_sessions_history_and_survive_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    for event in [
        reading(1, 182_000),
        ProviderEvent::CompactionStarted,
        failed("Conversation too long to summarise"),
        ProviderEvent::CompactionStarted,
        summarised("The parser work is half done."),
        reading(2, 31_000),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let before = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    let recorded = compactions(&before)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        measured(&before),
        [
            (ActivityStatus::Failed, Some(182_000), None),
            (ActivityStatus::Completed, Some(182_000), Some(31_000)),
        ],
        "both attempts are recorded, each as it settled and with the Context Fill read around it"
    );
    assert_eq!(
        summaries(&before),
        [
            (None, false),
            (Some("The parser work is half done."), false)
        ],
        "the completed one keeps the summary it left"
    );
    assert_eq!(before.turns[0].status, TurnStatus::Completed);
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, _provider) = crate::provider_support::ControlledProvider::new();
    let restarted = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            runtime,
        ),
    )
    .await
    .expect("the server restarts in time")
    .expect("respawn server");
    let restored = read_session(restarted.descriptor(), session_id).await;
    assert_eq!(
        compactions(&restored),
        recorded.iter().collect::<Vec<_>>(),
        "Compactions are history like any other Activity"
    );
    let positions = |snapshot: &SessionSnapshot| {
        recorded
            .iter()
            .map(|compaction| {
                snapshot.transcript.iter().position(|item| {
                    *item
                        == TranscriptItem::Activity {
                            activity_id: compaction.id(),
                        }
                })
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        positions(&restored),
        positions(&before),
        "each keeps its place in the Transcript"
    );
    restarted.shutdown().await.expect("shut down server");
}

/// A Session whose first Turn has settled, leaving it idle with its Provider
/// connection open — the state a Compaction request is made from.
async fn idle_session(state_dir: &std::path::Path, channel: &str) -> WorkingTurn {
    let fixture = working_turn(state_dir, channel).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    session_where(
        &fixture,
        fixture.session_id,
        "the first Turn settles",
        |snapshot| turn_settled(snapshot, 0) && snapshot.session.working_since.is_none(),
    )
    .await;
    fixture
}

/// Asks for a Compaction of the fixture's Session and waits for the request
/// to reach its Provider, which takes it.
async fn request_compaction(fixture: &mut WorkingTurn) -> SessionSnapshot {
    let response = compact(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        None,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "the idle Session takes the request: {:?}",
        response.text().await
    );
    let request = timeout(
        PROGRESS_DEADLINE,
        fixture.provider_session.next_compaction(),
    )
    .await
    .expect("the request reaches the Provider");
    let requested = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert_eq!(
        request.input().turn_id,
        requested.turns[1].id,
        "the Provider is asked to compact in the Turn the request began"
    );
    assert_eq!(request.input().instructions, None);
    request.succeed();
    requested
}

fn user_messages(snapshot: &SessionSnapshot) -> usize {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count()
}

async fn refusal(response: reqwest::Response) -> (StatusCode, SessionErrorCode) {
    let status = response.status();
    let error = response
        .json::<SessionError>()
        .await
        .expect("a refusal carries a typed Session error");
    (status, error.code)
}

/// The Standing inputs the Session's listing carries, from which a Client
/// reads whether its latest Turn left it Failed.
async fn listed_latest_turn(fixture: &WorkingTurn) -> Option<TurnStatus> {
    let listed = fixture
        .client
        .get(format!(
            "{}/v1/sessions",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session listing");
    listed.into_iter().find_map(|item| match item {
        SessionListItem::Readable(summary) if summary.session.id == fixture.session_id => summary
            .standing_inputs
            .latest_turn
            .map(|latest| latest.status),
        _ => None,
    })
}

#[tokio::test]
async fn a_compaction_request_begins_a_turn_of_its_own_that_settles_as_its_manual_compaction_does()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-requested-test").await;
    let session_id = fixture.session_id;
    let before = read_session(fixture.server.descriptor(), session_id).await;

    let requested = request_compaction(&mut fixture).await;
    let turn = &requested.turns[1];
    assert!(
        turn.compaction_requested && turn.prompt_id.is_none() && !turn.is_continuation(),
        "the request begins a Turn of its own, no Continuation: {turn:?}"
    );
    assert_eq!(turn.status, TurnStatus::Active);
    assert_eq!(
        turn.agent.as_ref().map(|agent| &agent.agent),
        before.turns[0].agent.as_ref().map(|agent| &agent.agent),
        "the Turn runs under the Session's own Agent"
    );
    assert!(
        requested.session.working_since.is_some(),
        "the Session is Working from the moment the request is taken"
    );
    assert_eq!(
        user_messages(&requested),
        user_messages(&before),
        "no user Message is drawn for a Suru command"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let compacting = read_session(fixture.server.descriptor(), session_id).await;
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            ..
        },
    ] = compactions(&compacting)[..]
    else {
        panic!("one Compaction opens: {:?}", compacting.activities);
    };
    assert_eq!(*turn_id, requested.turns[1].id);
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(
        *trigger,
        CompactionTrigger::Manual,
        "a Compaction in a Turn begun by request is manual"
    );

    let usage = Usage {
        fresh_input_tokens: Some(1_446),
        output_tokens: Some(1_044),
        ..Usage::default()
    };
    for event in [
        completed(Some(182_000), Some(31_000)),
        ProviderEvent::Usage {
            usage: usage.clone(),
            cost: None,
        },
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let compacted = read_session(fixture.server.descriptor(), session_id).await;
    assert_eq!(compaction_statuses(&compacted), [ActivityStatus::Completed]);
    assert_eq!(
        compacted.turns[1].status,
        TurnStatus::Active,
        "the Turn Settles at the Provider's own boundary"
    );
    assert!(compacted.session.working_since.is_some());

    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(settled.turns[1].usage, Some(usage), "its Usage is recorded");
    let compaction_turn = settled.turns[1].id;
    assert_eq!(
        settled
            .transcript
            .iter()
            .filter(|item| match item {
                TranscriptItem::Activity { activity_id } =>
                    settled.activities.iter().any(|activity| {
                        activity.id() == *activity_id && activity.turn_id() == compaction_turn
                    }),
                TranscriptItem::Message { message_id } => settled
                    .messages
                    .iter()
                    .any(|message| message.id == *message_id && message.turn_id == compaction_turn),
            })
            .collect::<Vec<_>>(),
        vec![&TranscriptItem::Activity {
            activity_id: compactions(&settled)[0].id()
        }],
        "the Compaction is the Turn's only content"
    );
    assert_eq!(
        settled.session.working_since, None,
        "the Session is idle again"
    );
    assert_eq!(
        listed_latest_turn(&fixture).await,
        Some(TurnStatus::Completed)
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_requested_compaction_that_fails_fails_its_turn_and_leaves_the_session_failed() {
    for (outcome, error) in [
        (
            vec![
                failed("No messages to compact"),
                ProviderEvent::TurnCompleted,
            ],
            Some("No messages to compact"),
        ),
        (
            vec![
                ProviderEvent::CompactionStarted,
                failed("Conversation too long to summarise"),
                // A Provider restating the failure in its own words is the
                // same occasion: the Turn holds the one Compaction it began.
                failed("Error during compaction: Conversation too long to summarise"),
                ProviderEvent::TurnCompleted,
            ],
            Some("Conversation too long to summarise"),
        ),
        // A Provider ending the Turn mid-Compaction left it unfinished.
        (
            vec![
                ProviderEvent::CompactionStarted,
                ProviderEvent::TurnCompleted,
            ],
            None,
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let mut fixture = idle_session(state_dir.path(), "compaction-requested-fails-test").await;
        let session_id = fixture.session_id;
        request_compaction(&mut fixture).await;
        for event in outcome {
            fixture.provider_session.emit(event);
        }
        let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
            turn_settled(snapshot, 1)
        })
        .await;
        assert_eq!(
            settled.turns[1].status,
            TurnStatus::Failed,
            "the Turn fails as its Compaction did, whatever the Provider's boundary said"
        );
        let [
            Activity::Compaction {
                status,
                trigger,
                error: recorded,
                ..
            },
        ] = compactions(&settled)[..]
        else {
            panic!("one Compaction is recorded: {:?}", settled.activities);
        };
        assert_eq!(*status, ActivityStatus::Failed);
        assert_eq!(*trigger, CompactionTrigger::Manual);
        assert_eq!(recorded.as_deref(), error);
        assert!(
            !settled.activities.iter().any(|activity| matches!(
                activity,
                Activity::Error { turn_id, .. } if *turn_id == settled.turns[1].id
            )),
            "the Compaction already says why, so nothing stands beside it: {:?}",
            settled.activities
        );
        assert_eq!(
            listed_latest_turn(&fixture).await,
            Some(TurnStatus::Failed),
            "the Session's Standing reads Failed"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_provider_ending_a_requested_compactions_turn_without_compacting_fails_it_saying_so() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-requested-silent-test").await;
    request_compaction(&mut fixture).await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(
        &fixture,
        fixture.session_id,
        "the Turn settles",
        |snapshot| turn_settled(snapshot, 1),
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    assert!(
        compactions(&settled).is_empty(),
        "no Compaction the Provider never reported is guessed at: {:?}",
        settled.activities
    );
    assert!(
        settled.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == settled.turns[1].id
                    && text.contains("without reporting a Compaction")
        )),
        "the Turn says why it failed: {:?}",
        settled.activities
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_requested_compaction_the_provider_refuses_to_begin_fails_its_turn_with_why() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-requested-refused-test").await;
    let response = compact(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    timeout(
        PROGRESS_DEADLINE,
        fixture.provider_session.next_compaction(),
    )
    .await
    .expect("the request reaches the Provider")
    .fail("the CLI is not reading its input");
    let settled = session_where(
        &fixture,
        fixture.session_id,
        "the Turn settles",
        |snapshot| turn_settled(snapshot, 1),
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    assert!(
        settled.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == settled.turns[1].id && text.contains("the CLI is not reading its input")
        )),
        "the Turn says why it never began: {:?}",
        settled.activities
    );
    assert_eq!(settled.session.working_since, None);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_request_is_refused_unless_the_session_is_idle_top_level_and_able() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "compaction-refusals-test").await;
    let session_id = fixture.session_id;
    let descriptor = fixture.server.descriptor().clone();

    assert_eq!(
        refusal(compact(&fixture.client, &descriptor, session_id, None).await).await,
        (StatusCode::CONFLICT, SessionErrorCode::WorkingSession),
        "a Session running a Turn is not idle"
    );

    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = session_where(&fixture, session_id, "the Subagent row opens", |snapshot| {
        !snapshot.activities.is_empty()
    })
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = *the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(
        refusal(compact(&fixture.client, &descriptor, child_id, None).await).await,
        (StatusCode::CONFLICT, SessionErrorCode::SubagentSession),
        "a Subagent's context is its Provider's alone to compact"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: Approval {
                id: ApprovalId::new(),
                subject: ApprovalSubject::Command {
                    command: "cargo nextest run".into(),
                    cwd: None,
                    actions: Vec::new(),
                },
                reason: None,
            },
            tool_activity_id: None,
        })
        .await;
    session_where(&fixture, session_id, "the Approval waits", |snapshot| {
        snapshot
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Approval { .. }))
    })
    .await;
    assert_eq!(
        refusal(compact(&fixture.client, &descriptor, session_id, None).await).await,
        (StatusCode::CONFLICT, SessionErrorCode::PendingIntervention),
        "a Session owing its reader an Intervention is not idle"
    );

    assert_eq!(
        refusal(
            compact(
                &fixture.client,
                &descriptor,
                session_id,
                Some("Keep the parser")
            )
            .await
        )
        .await,
        (
            StatusCode::CONFLICT,
            SessionErrorCode::CompactionInstructionsUnsupported
        ),
        "instructions no Provider takes are refused rather than dropped"
    );
    fixture.runtime.withdraw_manual_compaction();
    assert_eq!(
        refusal(compact(&fixture.client, &descriptor, session_id, None).await).await,
        (
            StatusCode::CONFLICT,
            SessionErrorCode::CompactionUnsupported
        ),
        "a Provider that compacts only when it chooses refuses the request"
    );
    assert_eq!(
        refusal(compact(&fixture.client, &descriptor, SessionId::new(), None).await).await,
        (StatusCode::NOT_FOUND, SessionErrorCode::SessionNotFound)
    );

    assert!(
        fixture.provider_session.try_next_compaction().is_none(),
        "no refused request reaches the Provider"
    );
    let refused = read_session(&descriptor, session_id).await;
    assert_eq!(refused.turns.len(), 1, "no refused request begins a Turn");
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_sent_during_a_requested_compaction_is_never_steered_into_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-requested-steer-test").await;
    let session_id = fixture.session_id;
    request_compaction(&mut fixture).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;

    let descriptor = fixture.server.descriptor().clone();
    let admit = |text: &str, delivery| {
        let request = fixture
            .client
            .post(format!(
                "{}/v1/sessions/{session_id}/prompts",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .json(&AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery,
            });
        async move {
            let admitted = request.send().await.expect("admit a Prompt");
            assert!(admitted.status().is_success());
            admitted
                .json::<suru::protocol::Prompt>()
                .await
                .expect("decode the admitted Prompt")
        }
    };
    admit("Now the lexer", PromptDelivery::Steer).await;
    // A queued Prompt promoted to steer has no Turn here to join either.
    let queued = admit("Then the printer", PromptDelivery::Queue).await;
    let promoted = fixture
        .client
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts/{}/promote",
            descriptor.base_url, queued.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("promote the queued Prompt");
    assert!(promoted.status().is_success());
    let compacting = read_session(fixture.server.descriptor(), session_id).await;
    assert!(
        fixture.provider_session.try_next_steer().is_none(),
        "a Turn begun by a Compaction request accepts no steer"
    );
    assert_eq!(
        compacting.turns.len(),
        2,
        "the Prompt waits for a Turn of its own"
    );
    fixture
        .provider_session
        .emit(completed(Some(182_000), Some(31_000)));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the Prompt begins the next Turn once the Compaction's settles");
    assert_eq!(next.prompt(), "Now the lexer");
    let settled = read_session(fixture.server.descriptor(), session_id).await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert!(
        !settled
            .messages
            .iter()
            .any(|message| message.turn_id == settled.turns[1].id),
        "nothing joined the Compaction's Turn"
    );
    next.succeed();
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_requested_compactions_turn_is_stored_apart_from_a_continuation_and_survives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-requested-restart-test";
    let mut fixture = idle_session(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    request_compaction(&mut fixture).await;
    for event in [
        ProviderEvent::CompactionStarted,
        completed(Some(182_000), Some(31_000)),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let before = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, _provider) = crate::provider_support::ControlledProvider::new();
    let restarted = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            runtime,
        ),
    )
    .await
    .expect("the server restarts in time")
    .expect("respawn server");
    let restored = read_session(restarted.descriptor(), session_id).await;
    assert_eq!(
        restored.turns, before.turns,
        "every Turn reads back as it was"
    );
    assert!(
        restored.turns[1].compaction_requested && !restored.turns[1].is_continuation(),
        "the Turn a request began is no Continuation once stored"
    );
    assert_eq!(
        compactions(&restored),
        compactions(&before),
        "its manual Compaction reads back as recorded"
    );
    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_request_is_refused_while_a_prompt_waits_for_its_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = crate::provider_support::ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "compaction-undelivered-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let created = crate::support::create_session(
        server.descriptor(),
        &suru::protocol::CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Map the parser".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        },
    )
    .await;
    // The Provider is still starting, so the Prompt has no Turn yet.
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    let waiting = read_session(server.descriptor(), created.session.id).await;
    assert!(waiting.turns.is_empty() && waiting.session.working_since.is_some());

    assert_eq!(
        refusal(
            compact(
                &reqwest::Client::new(),
                server.descriptor(),
                created.session.id,
                None
            )
            .await
        )
        .await,
        (StatusCode::CONFLICT, SessionErrorCode::WorkingSession),
        "a Session owing a Turn to a Prompt it admitted is not idle"
    );
    drop(start);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_requested_compaction_its_provider_measured_nothing_of_reads_the_sessions_context_fill() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-requested-fill-test").await;
    let session_id = fixture.session_id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(1, 182_000))
        .await;
    request_compaction(&mut fixture).await;
    for event in [
        ProviderEvent::CompactionStarted,
        completed(None, None),
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        measured(&settled),
        [(ActivityStatus::Completed, Some(182_000), None)],
        "the Context Fill last read before the request stands for its before"
    );

    fixture.provider_session.emit(reading(1, 31_000));
    let measured_after = session_where(
        &fixture,
        session_id,
        "the next reading measures the Compaction",
        |snapshot| measured(snapshot)[0].2.is_some(),
    )
    .await;
    assert_eq!(
        measured(&measured_after),
        [(ActivityStatus::Completed, Some(182_000), Some(31_000))],
        "the first reading after its Turn Settled stands for its after"
    );
    assert_eq!(
        compactions(&measured_after)
            .iter()
            .map(|compaction| match compaction {
                Activity::Compaction { trigger, .. } => *trigger,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        [CompactionTrigger::Manual]
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_native_turn_beginning_before_a_requested_compaction_is_asked_for_takes_its_place() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "compaction-requested-wake-test").await;
    let session_id = fixture.session_id;
    let descriptor = fixture.server.descriptor().clone();
    // A Watch outlives the Turn, leaving the Session idle but Monitoring.
    for event in [
        ProviderEvent::WatchStarted {
            watch_id: suru::provider::ProviderWatchId::new("monitor-1"),
            description: "Watch the build".to_owned(),
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    session_where(
        &fixture,
        session_id,
        "the Session is Monitoring",
        |snapshot| {
            turn_settled(snapshot, 0)
                && snapshot.session.working_since.is_none()
                && snapshot.session.monitoring_since.is_some()
        },
    )
    .await;

    // Hold the actor inside a Watch stop, so that both what follows wait for
    // it in a fixed order: the request's command, then the Provider's own
    // native turn, which the actor reads first.
    let (stopped, ()) = tokio::join!(interrupt(&fixture.client, &descriptor, session_id), async {
        let stop = timeout(
            PROGRESS_DEADLINE,
            fixture.provider_session.next_watches_stop(),
        )
        .await
        .expect("the interrupt holds the actor in a Watch stop");
        let response = compact(&fixture.client, &descriptor, session_id, None).await;
        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "a Monitoring Session is idle, so the request is taken"
        );
        fixture
            .provider_session
            .emit(ProviderEvent::ContinuationStarted {
                selection: controlled_selection("gpt-subagent", "high", "fast"),
            });
        stop.succeed();
    });
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);

    let woken = session_where(
        &fixture,
        session_id,
        "the native turn stands in a Continuation",
        |snapshot| snapshot.turns.len() == 3,
    )
    .await;
    assert_eq!(
        woken.turns[1].status,
        TurnStatus::Failed,
        "the request the Agent's own turn overtook fails"
    );
    assert!(
        woken.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == woken.turns[1].id && text.contains("before its Provider")
        )),
        "and says why: {:?}",
        woken.activities
    );
    assert!(
        compactions(&woken).is_empty(),
        "nothing was compacted: {:?}",
        woken.activities
    );
    assert!(
        woken.turns[2].is_continuation() && woken.turns[2].status == TurnStatus::Active,
        "the native turn keeps the Continuation it began: {:?}",
        woken.turns
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    assert!(
        fixture.provider_session.try_next_compaction().is_none(),
        "the request it overtook is never asked of the Provider"
    );
    let settled = session_where(
        &fixture,
        session_id,
        "the Continuation settles",
        |snapshot| turn_settled(snapshot, 2),
    )
    .await;
    assert_eq!(
        settled.turns[2].status,
        TurnStatus::Completed,
        "the Continuation settles on the Provider's own boundary"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

/// Interrupts the fixture's Session, and has its Provider acknowledge the
/// interrupt.
async fn interrupt_acknowledged(fixture: &mut WorkingTurn) {
    let (response, ()) = tokio::join!(
        interrupt(
            &fixture.client,
            fixture.server.descriptor(),
            fixture.session_id
        ),
        async {
            timeout(PROGRESS_DEADLINE, fixture.provider_session.next_interrupt())
                .await
                .expect("the interrupt reaches the Provider")
                .succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// Whether `snapshot` holds an Error standing in the Turn at `turn`.
fn turn_has_error(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot.activities.iter().any(|activity| {
        matches!(
            activity,
            Activity::Error { turn_id, .. } if *turn_id == snapshot.turns[turn].id
        )
    })
}

#[tokio::test]
async fn interrupting_a_requested_compaction_settles_it_and_its_turn_interrupted_measuring_nothing()
{
    // Each way a Provider may end the work once it has acknowledged Suru's
    // interrupt: reporting the cancelled Compaction as a failure, as Claude
    // does, or leaving it running; and closing the Turn as interrupted, or
    // with a success that compacted nothing, as Claude's `result` does.
    for (described, ending) in [
        (
            "the cancellation reported failed, then a completed boundary",
            vec![
                failed("API Error: Request was aborted."),
                ProviderEvent::TurnCompleted,
            ],
        ),
        (
            "the cancellation reported failed, then an interrupted boundary",
            vec![
                failed("API Error: Request was aborted."),
                ProviderEvent::TurnInterrupted,
            ],
        ),
        (
            "the Compaction left running, then a completed boundary",
            vec![ProviderEvent::TurnCompleted],
        ),
        (
            "the Compaction left running, then an interrupted boundary",
            vec![ProviderEvent::TurnInterrupted],
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let mut fixture =
            idle_session(state_dir.path(), "compaction-requested-interrupt-test").await;
        let session_id = fixture.session_id;
        fixture
            .provider_session
            .emit_and_wait_until_observed(reading(1, 182_000))
            .await;
        request_compaction(&mut fixture).await;
        fixture
            .provider_session
            .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
            .await;
        let compacting = read_session(fixture.server.descriptor(), session_id).await;
        assert_eq!(
            measured(&compacting),
            [(ActivityStatus::Active, Some(182_000), None)],
            "{described}: while it runs, the Compaction holds the reading before it"
        );

        interrupt_acknowledged(&mut fixture).await;
        for event in ending {
            fixture.provider_session.emit(event);
        }
        let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
            turn_settled(snapshot, 1) && snapshot.session.working_since.is_none()
        })
        .await;
        assert_eq!(
            settled.turns[1].status,
            TurnStatus::Interrupted,
            "{described}: the Turn Settles as its Compaction did, stopped"
        );
        let [
            Activity::Compaction {
                turn_id,
                status,
                trigger,
                error,
                ..
            },
        ] = compactions(&settled)[..]
        else {
            panic!(
                "{described}: one Compaction is recorded: {:?}",
                settled.activities
            );
        };
        assert_eq!(*turn_id, settled.turns[1].id);
        assert_eq!(
            (*status, *trigger, error),
            (
                ActivityStatus::Interrupted,
                CompactionTrigger::Manual,
                &None
            ),
            "{described}: the stop Suru asked for is no failure"
        );
        assert_eq!(
            measured(&settled),
            [(ActivityStatus::Interrupted, None, None)],
            "{described}: a stopped Compaction left the context as it was, so it carries no \
             Context Fill, not even the reading it began from"
        );
        assert!(
            !turn_has_error(&settled, 1),
            "{described}: nothing stands beside a stop: {:?}",
            settled.activities
        );
        assert_eq!(
            listed_latest_turn(&fixture).await,
            Some(TurnStatus::Interrupted),
            "{described}: an Interrupted Turn leaves the Session no Failed Standing"
        );

        // The next reading is no after of the Compaction that was stopped,
        // and the idle Session takes the next Prompt as a Turn of its own.
        fixture
            .provider_session
            .emit_and_wait_until_observed(reading(1, 150_000))
            .await;
        admit_prompt(fixture.server.descriptor(), session_id, "Now the lexer").await;
        let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
            .await
            .expect("the Prompt begins the next Turn");
        assert_eq!(next.prompt(), "Now the lexer");
        next.succeed();
        fixture.provider_session.emit(ProviderEvent::TurnCompleted);
        let answered = session_where(&fixture, session_id, "the next Turn settles", |snapshot| {
            turn_settled(snapshot, 2)
        })
        .await;
        assert_eq!(answered.turns[2].status, TurnStatus::Completed);
        assert_eq!(
            measured(&answered),
            [(ActivityStatus::Interrupted, None, None)],
            "{described}: no later reading is the stopped Compaction's after"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_requested_compaction_interrupted_before_its_provider_reported_compacting_holds_none() {
    for ending in [ProviderEvent::TurnCompleted, ProviderEvent::TurnInterrupted] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let mut fixture = idle_session(
            state_dir.path(),
            "compaction-requested-early-interrupt-test",
        )
        .await;
        let session_id = fixture.session_id;
        request_compaction(&mut fixture).await;

        interrupt_acknowledged(&mut fixture).await;
        fixture.provider_session.emit(ending.clone());
        let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
            turn_settled(snapshot, 1) && snapshot.session.working_since.is_none()
        })
        .await;
        assert_eq!(
            settled.turns[1].status,
            TurnStatus::Interrupted,
            "a Turn stopped before anything was compacted is stopped, not failed: {ending:?}"
        );
        assert!(
            compactions(&settled).is_empty(),
            "no Compaction the Provider never reported is guessed at: {:?}",
            settled.activities
        );
        assert!(
            !turn_has_error(&settled, 1),
            "nothing stands beside a stop: {:?}",
            settled.activities
        );
        assert_eq!(
            listed_latest_turn(&fixture).await,
            Some(TurnStatus::Interrupted)
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_summary_is_stored_as_provider_text_under_a_cap_that_flags_what_it_cut() {
    /// Suru's cap on one stored summary, in characters.
    const CAP: usize = 64 * 1024;
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-summary-test").await;
    let session_id = fixture.session_id;
    let oversized = format!("{}{}", "a".repeat(CAP), "dropped for good");
    for event in [
        ProviderEvent::CompactionStarted,
        summarised("\n  The \u{1b}[1mparser\u{1b}[0m work is half done.\u{1b}]0;title\u{7}\n\n"),
        ProviderEvent::CompactionStarted,
        summarised(&oversized),
        ProviderEvent::CompactionStarted,
        summarised(" \n\t "),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    let [first, second, third] = summaries(&settled)[..] else {
        panic!("three Compactions are recorded: {:?}", settled.activities);
    };
    assert_eq!(
        first,
        (
            Some("The \u{1b}[1mparser\u{1b}[0m work is half done."),
            false
        ),
        "a summary is normalized like any Provider text, keeping its formatting and dropping \
         what a terminal would act on, with the blank lines around it trimmed"
    );
    let (Some(capped), true) = second else {
        panic!("a summary past the cap is flagged as cut: {second:?}");
    };
    assert_eq!(
        capped.chars().count(),
        CAP,
        "the cap keeps as much of the summary as it allows"
    );
    assert!(
        !capped.contains("dropped"),
        "what the cap dropped is gone from the stored summary, which carries no marker of Suru's"
    );
    assert_eq!(
        third,
        (None, false),
        "a summary with nothing to read is none"
    );
    assert!(
        settled
            .messages
            .iter()
            .all(|message| !message.content.contains("parser")),
        "a summary is no Message: {:?}",
        settled.messages
    );
    fixture.server.shutdown().await.expect("shut down server");
}

/// Sends `text` to the fixture's Session the way a client submits a Prompt by
/// default, to steer whatever Turn is running, answering the Prompt the
/// Session admitted.
async fn admit_steer(fixture: &WorkingTurn, text: &str) -> Prompt {
    admit_steer_to(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        text,
    )
    .await
}

/// [`admit_steer`] by the parts of a fixture, for a test holding the rest of
/// it elsewhere.
async fn admit_steer_to(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    text: &str,
) -> Prompt {
    let response = client
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("admit a Prompt");
    assert_eq!(response.status(), StatusCode::CREATED);
    response
        .json::<Prompt>()
        .await
        .expect("decode the admitted Prompt")
}

/// Whether `prompt` stands in `snapshot` held: admitted and still owed a Turn,
/// which every client draws as the user Message it will become.
fn held(snapshot: &SessionSnapshot, prompt: PromptId) -> bool {
    snapshot
        .prompts
        .iter()
        .any(|admitted| admitted.id == prompt && admitted.status == PromptStatus::Pending)
        && !snapshot
            .turns
            .iter()
            .any(|turn| turn.prompt_id == Some(prompt))
}

/// The status `prompt` stands at in `snapshot`.
fn prompt_status(snapshot: &SessionSnapshot, prompt: PromptId) -> PromptStatus {
    snapshot
        .prompts
        .iter()
        .find(|admitted| admitted.id == prompt)
        .expect("the Session keeps every Prompt it admitted")
        .status
}

/// A requested Compaction of the fixture's Session at work, with a Prompt sent
/// while it runs held behind it: the Turn the request began, and the Prompt.
async fn held_behind_a_requested_compaction(
    fixture: &mut WorkingTurn,
    text: &str,
) -> (SessionSnapshot, Prompt) {
    let requested = request_compaction(fixture).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let prompt = admit_steer(fixture, text).await;
    let holding = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        held(&holding, prompt.id),
        "the Prompt is held, owed the next Turn: {:?}",
        holding.prompts
    );
    assert_eq!(holding.turns.len(), 2, "no Turn begins over the Compaction");
    assert_eq!(
        user_messages(&holding),
        user_messages(&requested),
        "nothing joins the Compaction's Turn"
    );
    assert_eq!(
        holding.session.working_since, requested.session.working_since,
        "the Session is Working as it was"
    );
    (requested, prompt)
}

/// After the Session's Turns have settled, asks it for one more and answers
/// the Prompt the Provider was next asked to begin a Turn with, which shows
/// whether any Prompt held before reached it.
async fn next_turn_begun_after(fixture: &mut WorkingTurn) -> String {
    let follow_up = admit_steer(fixture, "Start over on the lexer").await;
    let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("an idle Session begins a Turn for a Prompt");
    let prompt = next.prompt().to_owned();
    next.succeed();
    let begun = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        begun
            .turns
            .iter()
            .any(|turn| turn.prompt_id == Some(follow_up.id)),
        "the follow-up begins a Turn: {:?}",
        begun.turns
    );
    prompt
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_begins_the_next_turn_working_throughout() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-held-prompt-test";
    let mut fixture = idle_session(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    let (requested, prompt) =
        held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;
    let working_since = requested.session.working_since;
    assert_eq!(
        working_since, requested.turns[1].started_at,
        "Working from the moment the request began its Turn"
    );

    // Every reading from here until the held Prompt's Turn begins.
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the Session");
    let SessionEvent::Snapshot(subscribed) = timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("the Session's snapshot arrives")
        .expect("the Session stream stays open")
        .expect("the snapshot is valid")
    else {
        panic!("a subscription opens on a snapshot");
    };
    assert!(held(&subscribed, prompt.id));

    fixture
        .provider_session
        .emit(completed(Some(182_000), Some(31_000)));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the held Prompt begins the next Turn once the Compaction completes");
    assert_eq!(next.prompt(), "Now the lexer");
    assert!(
        fixture.provider_session.try_next_steer().is_none(),
        "the held Prompt was never delivered as a steer"
    );

    let mut working = Vec::new();
    loop {
        let update = crate::support::next_session_update(&mut subscription).await;
        working.extend(update.changes.iter().filter_map(|change| match change {
            SessionChange::SessionWorkingChanged { working_since } => Some(*working_since),
            _ => None,
        }));
        if update.changes.iter().any(|change| {
            matches!(change, SessionChange::TurnAdded { turn } if turn.prompt_id == Some(prompt.id))
        }) {
            break;
        }
    }
    assert_eq!(
        working,
        Vec::new(),
        "Working never changes from the Compaction's Turn to the held Prompt's"
    );
    let begun = read_session(fixture.server.descriptor(), session_id).await;
    assert_eq!(begun.turns[1].status, TurnStatus::Completed);
    assert!(
        !begun
            .messages
            .iter()
            .any(|message| message.turn_id == begun.turns[1].id),
        "nothing joined the Compaction's Turn"
    );
    assert_eq!(begun.turns[2].prompt_id, Some(prompt.id));
    assert_eq!(begun.turns[2].status, TurnStatus::Active);
    assert_eq!(prompt_status(&begun, prompt.id), PromptStatus::Delivered);
    assert_eq!(begun.session.working_since, working_since);
    next.succeed();
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_that_fails_is_withdrawn() {
    for (described, outcome) in [
        (
            "the Compaction fails",
            vec![
                failed("Conversation too long to summarise"),
                ProviderEvent::TurnCompleted,
            ],
        ),
        (
            "the Provider ends the Turn mid-Compaction",
            vec![ProviderEvent::TurnCompleted],
        ),
        (
            "the Provider fails the Turn",
            vec![ProviderEvent::TurnFailed {
                message: "the CLI exited".to_owned(),
            }],
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let mut fixture = idle_session(state_dir.path(), "compaction-held-fails-test").await;
        let session_id = fixture.session_id;
        let (_, prompt) = held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;

        for event in outcome {
            fixture.provider_session.emit(event);
        }
        let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
            turn_settled(snapshot, 1)
        })
        .await;
        assert_eq!(settled.turns[1].status, TurnStatus::Failed, "{described}");
        assert_eq!(
            prompt_status(&settled, prompt.id),
            PromptStatus::Cancelled,
            "when {described}, the held Prompt is withdrawn as the Turn settles"
        );
        assert_eq!(
            settled.session.working_since, None,
            "when {described}, nothing is left Working"
        );
        assert_eq!(settled.turns.len(), 2, "{described}: no Turn begins for it");
        assert_eq!(
            next_turn_begun_after(&mut fixture).await,
            "Start over on the lexer",
            "when {described}, the withdrawn Prompt never reaches the Provider"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_the_provider_refuses_is_withdrawn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-held-refused-test").await;
    let session_id = fixture.session_id;
    let response = compact(
        &fixture.client,
        fixture.server.descriptor(),
        session_id,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let request = timeout(
        PROGRESS_DEADLINE,
        fixture.provider_session.next_compaction(),
    )
    .await
    .expect("the request reaches the Provider");
    let prompt = admit_steer(&fixture, "Now the lexer").await;
    request.fail("the CLI is not reading its input");

    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    assert_eq!(prompt_status(&settled, prompt.id), PromptStatus::Cancelled);
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_that_is_interrupted_is_withdrawn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-held-interrupted-test").await;
    let session_id = fixture.session_id;
    let (_, prompt) = held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;

    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), session_id),
        async {
            timeout(PROGRESS_DEADLINE, fixture.provider_session.next_interrupt())
                .await
                .expect("the interrupt reaches the Provider")
                .succeed();
        }
    );
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "the interrupt stops the Compaction, not the Prompt held behind it"
    );
    let interrupting = read_session(fixture.server.descriptor(), session_id).await;
    assert!(
        held(&interrupting, prompt.id),
        "the Prompt stays held until the Compaction settles: {:?}",
        interrupting.prompts
    );
    for event in [
        failed("Request was aborted."),
        ProviderEvent::TurnInterrupted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Interrupted);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Interrupted]);
    assert_eq!(
        prompt_status(&settled, prompt.id),
        PromptStatus::Cancelled,
        "the held Prompt is withdrawn as the Turn settles"
    );
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer",
        "the withdrawn Prompt never reaches the Provider"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_request_a_native_turn_overtook_is_withdrawn_leaving_that_turn_be() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "compaction-held-overtaken-test").await;
    let session_id = fixture.session_id;
    let descriptor = fixture.server.descriptor().clone();
    // A Watch outlives the Turn, leaving the Session idle but Monitoring.
    for event in [
        ProviderEvent::WatchStarted {
            watch_id: suru::provider::ProviderWatchId::new("monitor-1"),
            description: "Watch the build".to_owned(),
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    session_where(
        &fixture,
        session_id,
        "the Session is Monitoring",
        |snapshot| turn_settled(snapshot, 0) && snapshot.session.monitoring_since.is_some(),
    )
    .await;

    // Hold the actor inside a Watch stop, so that what follows waits for it
    // in a fixed order: the request's command, then the held Prompt's, then
    // the Provider's own native turn, which the actor reads first.
    let client = fixture.client.clone();
    let (stopped, prompt) = tokio::join!(interrupt(&client, &descriptor, session_id), async {
        let stop = timeout(
            PROGRESS_DEADLINE,
            fixture.provider_session.next_watches_stop(),
        )
        .await
        .expect("the interrupt holds the actor in a Watch stop");
        let response = compact(&client, &descriptor, session_id, None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let prompt = admit_steer_to(&client, &descriptor, session_id, "Now the lexer").await;
        fixture
            .provider_session
            .emit(ProviderEvent::ContinuationStarted {
                selection: controlled_selection("gpt-subagent", "high", "fast"),
            });
        stop.succeed();
        prompt
    });
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);

    let woken = session_where(
        &fixture,
        session_id,
        "the native turn stands in a Continuation",
        |snapshot| snapshot.turns.len() == 3,
    )
    .await;
    assert_eq!(woken.turns[1].status, TurnStatus::Failed);
    assert_eq!(
        prompt_status(&woken, prompt.id),
        PromptStatus::Cancelled,
        "the Prompt held for a Compaction that never ran is withdrawn with it"
    );
    // The actor reads the Provider's output only between the commands queued
    // ahead of it — and not at all while it waits on a stop it asked for.
    timeout(
        PROGRESS_DEADLINE,
        fixture
            .provider_session
            .emit_and_wait_until_observed(ProviderEvent::TurnCompleted),
    )
    .await
    .expect("the actor reads the native turn's end rather than stopping that turn");
    let settled = session_where(
        &fixture,
        session_id,
        "the Continuation settles",
        |snapshot| turn_settled(snapshot, 2),
    )
    .await;
    assert_eq!(
        settled.turns[2].status,
        TurnStatus::Completed,
        "the Agent's own turn runs to its own boundary"
    );
    assert!(
        fixture.provider_session.try_next_interrupt().is_none(),
        "nothing withdrawn stops the Agent's own turn"
    );
    assert_eq!(
        settled.turns.len(),
        3,
        "no Turn begins for the withdrawn Prompt"
    );
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer"
    );
    fixture.server.shutdown().await.expect("shut down server");
}
