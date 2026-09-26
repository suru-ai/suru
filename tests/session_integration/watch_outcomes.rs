//! Watch Outcomes at the provider-neutral seam (ADR 0030): a Watch whose
//! settling wakes the Agent records how it settled in the Turn it woke the
//! Agent into — the one active when it settled, or else the next to open —
//! while one that wakes nothing records nothing, and a recorded outcome is
//! history like any other Activity.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    WorkingTurn, controlled_selection, read_session, read_session_until, working_turn,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, PromptDelivery, PromptId,
        RuntimeDescriptor, SessionId, SessionSnapshot, TranscriptItem, TurnId, TurnStatus,
        WatchOutcomeStatus,
    },
    provider::{
        ProviderEvent, ProviderEventAttribution, ProviderSubagentId, ProviderSubagentStatus,
        ProviderWatchId, ProviderWatchOutcome,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

fn watch_started(watch: &str, description: &str) -> ProviderEvent {
    ProviderEvent::WatchStarted {
        watch_id: ProviderWatchId::new(watch),
        description: description.to_owned(),
    }
}

/// A Watch settling the way one that woke the Agent does.
fn watch_woke_agent(
    watch: &str,
    outcome: ProviderWatchOutcome,
    summary: Option<&str>,
) -> ProviderEvent {
    ProviderEvent::WatchSettled {
        watch_id: ProviderWatchId::new(watch),
        outcome,
        summary: summary.map(ToOwned::to_owned),
        woke_agent: true,
    }
}

/// A Watch settling with nothing woken: stopped by an interrupt, or lost with
/// its Provider process.
fn watch_woke_nothing(watch: &str, outcome: ProviderWatchOutcome) -> ProviderEvent {
    ProviderEvent::WatchSettled {
        watch_id: ProviderWatchId::new(watch),
        outcome,
        summary: None,
        woke_agent: false,
    }
}

async fn session_where(
    fixture: &WorkingTurn,
    described: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        described,
        predicate,
    )
    .await
}

fn turn_settled(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot
        .turns
        .get(turn)
        .is_some_and(|turn| turn.status != TurnStatus::Active)
}

fn watch_outcomes(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::WatchOutcome { .. }))
        .collect()
}

/// The first Transcript entry belonging to `turn_id`: what the reader meets
/// at the head of that Turn.
fn head_of_turn(snapshot: &SessionSnapshot, turn_id: TurnId) -> Option<TranscriptItem> {
    snapshot.transcript.iter().copied().find(|item| match item {
        TranscriptItem::Message { message_id } => snapshot
            .messages
            .iter()
            .any(|message| message.id == *message_id && message.turn_id == turn_id),
        TranscriptItem::Activity { activity_id } => snapshot
            .activities
            .iter()
            .any(|activity| activity.id() == *activity_id && activity.turn_id() == turn_id),
    })
}

/// The first Activity belonging to `turn_id` in Transcript order.
fn first_activity_of_turn(snapshot: &SessionSnapshot, turn_id: TurnId) -> Option<&Activity> {
    snapshot.transcript.iter().find_map(|item| match item {
        TranscriptItem::Activity { activity_id } => snapshot
            .activities
            .iter()
            .find(|activity| activity.id() == *activity_id && activity.turn_id() == turn_id),
        TranscriptItem::Message { .. } => None,
    })
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
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt")
        .error_for_status()
        .expect("Prompt admission succeeds");
}

#[tokio::test]
async fn a_watch_settling_while_a_turn_is_active_records_its_watch_outcome_in_that_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "watch-outcome-in-active-turn-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture.provider_session.emit(watch_woke_agent(
        "task-tests",
        ProviderWatchOutcome::Completed,
        Some(r#"Background command "cargo test" completed (exit code 0)"#),
    ));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);

    let settled = session_where(&fixture, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        settled.turns.len(),
        1,
        "a Watch settling mid-Turn wakes the Agent into that Turn, not a Continuation"
    );
    let outcomes = watch_outcomes(&settled);
    assert_eq!(outcomes.len(), 1, "one Watch woke the Agent: {outcomes:?}");
    assert_eq!(
        outcomes[0],
        &Activity::WatchOutcome {
            id: outcomes[0].id(),
            turn_id: settled.turns[0].id,
            status: WatchOutcomeStatus::Completed,
            description: "cargo test".to_owned(),
            summary: Some(r#"Background command "cargo test" completed (exit code 0)"#.to_owned()),
        },
        "the Watch Outcome stands in the Turn that was active when the Watch settled"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_watch_waking_an_idle_session_heads_the_continuation_it_begins() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "watch-outcome-heads-continuation-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    session_where(&fixture, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    fixture.provider_session.emit(watch_woke_agent(
        "task-tests",
        ProviderWatchOutcome::Failed,
        Some(r#"Background command "cargo test" failed with exit code 1"#),
    ));
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageStarted);
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageDelta {
            content: "The tests failed; looking now.".to_owned(),
        });
    let woken = session_where(&fixture, "the Continuation streams", |snapshot| {
        snapshot
            .messages
            .iter()
            .any(|message| message.content == "The tests failed; looking now.")
    })
    .await;
    assert_eq!(woken.turns.len(), 2);
    assert_eq!(woken.turns[1].prompt_id, None, "the wake is a Continuation");
    let continuation = woken.turns[1].id;
    let outcome = first_activity_of_turn(&woken, continuation)
        .expect("the Continuation holds the Watch Outcome");
    assert!(matches!(
        outcome,
        Activity::WatchOutcome {
            status: WatchOutcomeStatus::Failed,
            ..
        }
    ));
    assert_eq!(
        head_of_turn(&woken, continuation),
        Some(TranscriptItem::Activity {
            activity_id: outcome.id()
        }),
        "the Watch Outcome precedes everything the Agent did about it"
    );
    assert!(
        !watch_outcomes(&woken)
            .iter()
            .any(|activity| activity.turn_id() == woken.turns[0].id),
        "the settled Turn accepts no Watch Outcome (ADR 0015)"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_turn_that_wins_the_race_to_a_watchs_wake_begins_with_its_watch_outcome() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "watch-outcome-heads-prompt-turn-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    session_where(&fixture, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(watch_woke_agent(
            "task-tests",
            ProviderWatchOutcome::Completed,
            None,
        ))
        .await;

    admit_prompt(
        fixture.server.descriptor(),
        fixture.session_id,
        "How did the tests go?",
    )
    .await;
    timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the Prompt's Turn reaches the Provider")
        .succeed();
    let prompted = session_where(
        &fixture,
        "the Prompt's Turn holds the outcome",
        |snapshot| snapshot.turns.len() == 2 && !watch_outcomes(snapshot).is_empty(),
    )
    .await;
    assert!(prompted.turns[1].prompt_id.is_some());
    let outcome = first_activity_of_turn(&prompted, prompted.turns[1].id)
        .expect("the Prompt's Turn holds the Watch Outcome");
    assert_eq!(
        outcome,
        &Activity::WatchOutcome {
            id: outcome.id(),
            turn_id: prompted.turns[1].id,
            status: WatchOutcomeStatus::Completed,
            description: "cargo test".to_owned(),
            summary: None,
        },
        "the Turn that opens next is where the Agent hears of the wake"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn watches_that_are_stopped_or_lost_record_no_watch_outcome() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "watch-outcome-woke-nothing-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-stopped-mid-turn", "cargo build"));
    fixture.provider_session.emit(watch_woke_nothing(
        "task-stopped-mid-turn",
        ProviderWatchOutcome::Stopped,
    ));
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture
        .provider_session
        .emit(watch_started("monitor-1", "tail -f server.log"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    session_where(&fixture, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    fixture.provider_session.emit(watch_woke_nothing(
        "task-tests",
        ProviderWatchOutcome::Stopped,
    ));
    fixture
        .provider_session
        .emit(watch_woke_nothing("monitor-1", ProviderWatchOutcome::Lost));
    fixture
        .provider_session
        .emit(ProviderEvent::ContinuationStarted {
            selection: controlled_selection("gpt-subagent", "high", "fast"),
        });
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageStarted);
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageDelta {
            content: "Carrying on.".to_owned(),
        });
    let continued = session_where(&fixture, "a later Continuation streams", |snapshot| {
        snapshot
            .messages
            .iter()
            .any(|message| message.content == "Carrying on.")
    })
    .await;
    assert_eq!(continued.turns.len(), 2);
    assert_eq!(
        watch_outcomes(&continued),
        Vec::<&Activity>::new(),
        "Watches that woke nothing leave nothing for the Transcript to explain"
    );
    assert!(
        matches!(
            head_of_turn(&continued, continued.turns[1].id),
            Some(TranscriptItem::Message { .. })
        ),
        "the later Continuation begins with the Agent's own output"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_held_watch_outcome_is_dropped_when_the_session_is_interrupted_before_a_turn_opens() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "watch-outcome-dropped-test").await;
    // A surviving Subagent keeps the Session Working past its Turn, which is
    // what an interrupt with no Turn active reaches.
    fixture
        .provider_session
        .emit(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        });
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(watch_woke_agent(
            "task-tests",
            ProviderWatchOutcome::Completed,
            Some("Tests passed"),
        ))
        .await;

    let (response, ()) = tokio::join!(
        fixture
            .client
            .post(format!(
                "{}/v1/sessions/{}/interrupt",
                fixture.server.descriptor().base_url,
                fixture.session_id
            ))
            .bearer_auth(&fixture.server.descriptor().token)
            .send(),
        async {
            timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagents_stop(),
            )
            .await
            .expect("the interrupt reaches the surviving Subagent")
            .succeed();
        }
    );
    response
        .expect("send the interrupt")
        .error_for_status()
        .expect("the interrupt succeeds");
    session_where(
        &fixture,
        "the interrupt leaves the Session idle",
        |snapshot| snapshot.session.working_since.is_none(),
    )
    .await;

    admit_prompt(
        fixture.server.descriptor(),
        fixture.session_id,
        "Start something else",
    )
    .await;
    timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the Prompt's Turn reaches the Provider")
        .succeed();
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageStarted);
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageDelta {
            content: "On it.".to_owned(),
        });
    let prompted = session_where(&fixture, "the Prompt's Turn streams", |snapshot| {
        snapshot
            .messages
            .iter()
            .any(|message| message.content == "On it.")
    })
    .await;
    assert_eq!(prompted.turns.len(), 2);
    assert_eq!(
        watch_outcomes(&prompted),
        Vec::<&Activity>::new(),
        "an interrupt before any Turn opened drops the Watch Outcome it held"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_watch_owned_by_a_working_subagent_records_its_outcome_in_the_subagents_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "watch-outcome-in-subagent-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        });
    for event in [
        watch_started("task-grep", "rg provider"),
        watch_woke_agent(
            "task-grep",
            ProviderWatchOutcome::Completed,
            Some(r#"Background command "rg provider" completed"#),
        ),
    ] {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let child = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the Subagent's row names its Session");
    assert_eq!(
        watch_outcomes(&parent),
        Vec::<&Activity>::new(),
        "the Subagent's Watch woke the Subagent, not the owning Session's Agent"
    );
    let child = read_session(fixture.server.descriptor(), child).await;
    let outcomes = watch_outcomes(&child);
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(
        outcomes[0].turn_id(),
        child.turns[0].id,
        "the outcome lands in the Turn the Subagent is working in"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

/// The Subagent rows in `snapshot`'s Transcript, as (status, the Session each
/// leads into).
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<(ActivityStatus, SessionId)> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subagent {
                status, session_id, ..
            } => Some((*status, *session_id)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_settled_subagent_woken_by_its_own_watch_works_on_in_a_continuation_of_its_own_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "watch-wakes-settled-subagent-test").await;
    let provider = &fixture.provider_session;
    let subagent = ProviderSubagentId::new("task-1");
    let as_subagent = || ProviderEventAttribution::Subagent(subagent.clone());
    provider.emit(ProviderEvent::SubagentStarted {
        subagent_id: subagent.clone(),
        name: "Test".to_owned(),
        description: "Run the suite".to_owned(),
        delegation: Some("Run the suite in the background.".to_owned()),
    });
    provider
        .emit_attributed_and_wait_until_observed(
            as_subagent(),
            watch_started("task-tests", "cargo test"),
        )
        .await;
    provider.emit(ProviderEvent::SubagentCompleted {
        subagent_id: subagent.clone(),
        status: ProviderSubagentStatus::Completed,
    });
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let [(ActivityStatus::Completed, child)] = subagent_rows(&settled)[..] else {
        panic!("the spawn's stretch settled: {:?}", settled.activities);
    };

    provider
        .emit_attributed_and_wait_until_observed(
            as_subagent(),
            watch_woke_agent(
                "task-tests",
                ProviderWatchOutcome::Completed,
                Some(r#"Background command "cargo test" completed (exit code 0)"#),
            ),
        )
        .await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentWoken {
            subagent_id: subagent.clone(),
        })
        .await;
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "The suite passed.".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ] {
        provider
            .emit_attributed_and_wait_until_observed(as_subagent(), event)
            .await;
    }
    let woken = read_session(fixture.server.descriptor(), child).await;
    assert_eq!(woken.turns.len(), 2, "the wake began a second Turn");
    let continuation = &woken.turns[1];
    assert_eq!(continuation.status, TurnStatus::Active);
    assert_eq!(continuation.prompt_id, None);
    assert!(
        !woken
            .messages
            .iter()
            .any(|message| message.turn_id == continuation.id && message.role.delegator().is_some()),
        "no Delegation opens it, so it is a Continuation: {:?}",
        woken.messages
    );
    let outcome = first_activity_of_turn(&woken, continuation.id)
        .expect("the Continuation holds the Watch Outcome");
    assert!(
        matches!(
            outcome,
            Activity::WatchOutcome {
                status: WatchOutcomeStatus::Completed,
                summary: Some(summary),
                ..
            } if summary == r#"Background command "cargo test" completed (exit code 0)"#
        ),
        "{outcome:?}"
    );
    assert_eq!(
        head_of_turn(&woken, continuation.id),
        Some(TranscriptItem::Activity {
            activity_id: outcome.id()
        }),
        "the Watch Outcome heads the Continuation it woke the Subagent into"
    );
    assert!(
        woken
            .messages
            .iter()
            .any(|message| message.turn_id == continuation.id
                && message.content == "The suite passed."),
        "the woken Subagent's work lands in that Continuation"
    );
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert_eq!(
        parent.turns.len(),
        1,
        "nothing in the parent's conversation began a Turn there"
    );
    assert_eq!(
        subagent_rows(&parent),
        [(ActivityStatus::Completed, child)],
        "the wake adds no row, and the spawn's stays as it settled"
    );
    assert!(
        parent.session.working_since.is_some(),
        "the woken Subagent keeps its parent Working"
    );

    provider.emit(ProviderEvent::SubagentCompleted {
        subagent_id: subagent.clone(),
        status: ProviderSubagentStatus::Completed,
    });
    provider.emit(ProviderEvent::AgentMessageStarted);
    provider.emit(ProviderEvent::AgentMessageDelta {
        content: "The Subagent says the suite passed.".to_owned(),
    });
    provider.emit(ProviderEvent::AgentMessageCompleted);
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let followed_up = session_where(
        &fixture,
        "the parent's follow-up settles in a Continuation of its own",
        |snapshot| turn_settled(snapshot, 1),
    )
    .await;
    assert_eq!(followed_up.turns[1].prompt_id, None);
    assert!(
        followed_up
            .messages
            .iter()
            .any(|message| message.turn_id == followed_up.turns[1].id
                && message.content == "The Subagent says the suite passed.")
    );
    assert_eq!(
        subagent_rows(&followed_up),
        [(ActivityStatus::Completed, child)],
        "the woken stretch's settle publishes nothing on the parent's row"
    );
    assert_eq!(
        (
            followed_up.session.working_since,
            followed_up.session.monitoring_since
        ),
        (None, None)
    );
    let child = read_session(fixture.server.descriptor(), child).await;
    assert_eq!(
        child
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        [TurnStatus::Completed, TurnStatus::Completed],
        "the Subagent's settle closes its Continuation"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_watch_outcome_is_stored_with_the_sessions_history_and_survives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "watch-outcome-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture.provider_session.emit(watch_woke_agent(
        "task-tests",
        ProviderWatchOutcome::Failed,
        Some(r#"Background command "cargo test" failed with exit code 1"#),
    ));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let before = session_where(&fixture, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    let recorded = watch_outcomes(&before)
        .first()
        .copied()
        .cloned()
        .expect("the Watch Outcome is recorded");
    let session_id = fixture.session_id;
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
        watch_outcomes(&restored),
        vec![&recorded],
        "the Watch Outcome is history like any other Activity"
    );
    assert!(
        restored.transcript.contains(&TranscriptItem::Activity {
            activity_id: recorded.id()
        }),
        "it keeps its place in the Transcript"
    );
    restarted.shutdown().await.expect("shut down server");
}
