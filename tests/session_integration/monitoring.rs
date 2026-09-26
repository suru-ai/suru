//! Monitoring at the provider-neutral seam (ADR 0030): a Watch the Agent left
//! running keeps its Session Monitoring once nothing is Working, counted from
//! the later of when Working last ended and when the Watch started, carried on
//! the Session, its listing, and the catalog stream alike — and never stored,
//! so no Session is Monitoring after a restart. Interrupting a Session that is
//! only Monitoring asks its Provider to stop the Watches, and settles nothing.

use crate::server_support::{PROGRESS_DEADLINE, next_catalog_change_matching};
use crate::support::{WorkingTurn, open_catalog_stream_with_snapshot, read_session_until};
use suru::{
    protocol::{
        RuntimeDescriptor, SessionCatalogChange, SessionId, SessionListItem, SessionSnapshot,
        SessionSummary, SessionTimestamp, TurnStatus,
    },
    provider::{
        ProviderEvent, ProviderEventAttribution, ProviderSubagentId, ProviderSubagentStatus,
        ProviderWatchId, ProviderWatchOutcome,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

async fn working_turn(state_dir: &std::path::Path, channel: &str) -> WorkingTurn {
    crate::support::working_turn(state_dir, channel).await
}

/// The Session's row in the server's listing.
async fn listed(descriptor: &RuntimeDescriptor, session_id: SessionId) -> SessionSummary {
    let listing = reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("the listing answers")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Session listing");
    listing
        .into_iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == session_id)
                .cloned()
        })
        .expect("the Session is listed and readable")
}

fn watch_started(watch: &str, description: &str) -> ProviderEvent {
    ProviderEvent::WatchStarted {
        watch_id: ProviderWatchId::new(watch),
        description: description.to_owned(),
    }
}

/// A Watch settling the way one stopped on the Provider's own account does:
/// with nothing woken, so no output is owed after it.
fn watch_stopped(watch: &str) -> ProviderEvent {
    ProviderEvent::WatchSettled {
        watch_id: ProviderWatchId::new(watch),
        outcome: ProviderWatchOutcome::Stopped,
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

fn first_turn_settled(snapshot: &SessionSnapshot) -> bool {
    snapshot
        .turns
        .first()
        .is_some_and(|turn| turn.status != TurnStatus::Active)
}

fn settled_at(snapshot: &SessionSnapshot, turn: usize) -> SessionTimestamp {
    snapshot.turns[turn]
        .settled_at
        .expect("a settled Turn records when it Settled")
}

#[tokio::test]
async fn a_watch_outliving_its_turn_reads_monitoring_from_the_turns_settle_until_it_settles() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "monitoring-from-settle-test").await;
    let descriptor = fixture.server.descriptor().clone();
    let (_, mut catalog) = open_catalog_stream_with_snapshot(&descriptor).await;

    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    // Output the actor handles after the Watch, so reading it means the Watch
    // is recorded too.
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageStarted);
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageDelta {
            content: "Running the tests in the background.".to_owned(),
        });
    let during = session_where(&fixture, "the Turn streams past the Watch", |snapshot| {
        snapshot
            .messages
            .iter()
            .any(|message| message.content == "Running the tests in the background.")
    })
    .await;
    assert!(during.session.working_since.is_some());
    assert_eq!(
        during.session.monitoring_since, None,
        "a Session whose Turn is running is Working, however many Watches it has started"
    );

    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let monitoring = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    let turn_settled_at = settled_at(&monitoring, 0);
    assert_eq!(monitoring.session.working_since, None);
    assert_eq!(
        monitoring.session.monitoring_since,
        Some(turn_settled_at),
        "a Watch started mid-Turn is waited on only from the moment Working ended"
    );
    assert_eq!(
        listed(&descriptor, fixture.session_id)
            .await
            .session
            .monitoring_since,
        Some(turn_settled_at),
        "the listing carries the reading the Session does"
    );
    let announced = next_catalog_change_matching(&mut catalog, |change| {
        matches!(change, SessionCatalogChange::MonitoringChanged { .. })
    })
    .await;
    assert_eq!(
        announced,
        SessionCatalogChange::MonitoringChanged {
            session_id: fixture.session_id,
            monitoring_since: Some(turn_settled_at),
        }
    );

    fixture.provider_session.emit(watch_stopped("task-tests"));
    let settled = session_where(&fixture, "the Watch settles", |snapshot| {
        snapshot.session.monitoring_since.is_none()
    })
    .await;
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        settled.turns.len(),
        1,
        "a Watch settling is no output, so it opens no Continuation of its own"
    );
    assert_eq!(
        listed(&descriptor, fixture.session_id)
            .await
            .session
            .monitoring_since,
        None
    );
    let announced = next_catalog_change_matching(&mut catalog, |change| {
        matches!(change, SessionCatalogChange::MonitoringChanged { .. })
    })
    .await;
    assert_eq!(
        announced,
        SessionCatalogChange::MonitoringChanged {
            session_id: fixture.session_id,
            monitoring_since: None,
        },
        "the catalog stream announces Monitoring ending as it announced it beginning"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_watch_started_after_working_ended_reads_monitoring_from_its_own_start() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "monitoring-from-watch-test").await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let idle = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    assert_eq!(idle.session.monitoring_since, None);

    fixture
        .provider_session
        .emit(watch_started("monitor-1", "tail -f server.log"));
    let monitoring = session_where(&fixture, "the Watch is recorded", |snapshot| {
        snapshot.session.monitoring_since.is_some()
    })
    .await;
    assert!(
        monitoring.session.monitoring_since > Some(settled_at(&monitoring, 0)),
        "a Watch starting after Working ended is waited on from its own start: {:?}",
        monitoring.session
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn working_takes_precedence_over_a_live_watch_across_the_session_tree() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "working-over-monitoring-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture
        .provider_session
        .emit(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        });
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let delegated = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    assert!(
        delegated.session.working_since.is_some(),
        "a surviving Subagent keeps the Session Working after its Turn settles"
    );
    assert_eq!(
        delegated.session.monitoring_since, None,
        "a Session Working anywhere in its tree is not Monitoring"
    );

    fixture
        .provider_session
        .emit(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        });
    let monitoring = session_where(&fixture, "the Subagent settles", |snapshot| {
        snapshot.session.working_since.is_none()
    })
    .await;
    let child = monitoring
        .activities
        .iter()
        .find_map(|activity| match activity {
            suru::protocol::Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the Subagent's row names its Session");
    let child = crate::support::read_session(fixture.server.descriptor(), child).await;
    assert_eq!(
        monitoring.session.monitoring_since,
        Some(settled_at(&child, 0)),
        "Monitoring begins where Working last ended anywhere in the tree: the Subagent's settle"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_continuation_ends_monitoring_and_working_counts_afresh_from_its_start() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "continuation-ends-monitoring-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture
        .provider_session
        .emit(watch_started("monitor-1", "tail -f server.log"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let monitoring = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    assert!(monitoring.session.monitoring_since.is_some());

    // The test run finishing wakes the Agent, which works on in a
    // Continuation while the monitor stays live.
    fixture.provider_session.emit(ProviderEvent::WatchSettled {
        watch_id: ProviderWatchId::new("task-tests"),
        outcome: ProviderWatchOutcome::Completed,
        summary: Some("Tests passed".to_owned()),
        woke_agent: true,
    });
    fixture
        .provider_session
        .emit(ProviderEvent::AgentMessageStarted);
    let woken = session_where(&fixture, "the Continuation begins", |snapshot| {
        snapshot.turns.len() == 2
    })
    .await;
    assert_eq!(woken.turns[1].prompt_id, None, "the wake is a Continuation");
    assert_eq!(
        woken.session.working_since, woken.turns[1].started_at,
        "Working after a wake counts from the Continuation, not from the Turn before it"
    );
    assert_eq!(
        woken.session.monitoring_since, None,
        "a Session Working again is not Monitoring, though a Watch is still live"
    );

    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let again = session_where(&fixture, "the Continuation settles", |snapshot| {
        snapshot
            .turns
            .get(1)
            .is_some_and(|turn| turn.status != TurnStatus::Active)
    })
    .await;
    assert_eq!(
        again.session.monitoring_since,
        Some(settled_at(&again, 1)),
        "the monitor still live keeps the Session Monitoring once the Continuation settles"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restarted_server_shows_no_session_monitoring() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "monitoring-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    fixture
        .provider_session
        .emit(watch_started("monitor-1", "tail -f server.log"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let monitoring = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    assert!(monitoring.session.monitoring_since.is_some());
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
    let restored = crate::support::read_session(restarted.descriptor(), session_id).await;
    assert_eq!(
        restored.session.monitoring_since, None,
        "no Watch outlives the Provider process that ran it, so none keeps a Session Monitoring"
    );
    assert_eq!(restored.session.working_since, None);
    assert_eq!(
        listed(restarted.descriptor(), session_id)
            .await
            .session
            .monitoring_since,
        None
    );
    restarted.shutdown().await.expect("shut down server");
}

/// Asks the server to interrupt `session_id`, answering with the raw response
/// so what the interrupt did stays assertable.
async fn interrupt(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
    client
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
}

fn described(snapshot: &SessionSnapshot) -> Vec<&str> {
    snapshot
        .watches
        .iter()
        .map(|watch| watch.description.as_str())
        .collect()
}

#[tokio::test]
async fn interrupting_a_monitoring_session_stops_its_watches_and_ends_monitoring_with_no_new_turn()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "interrupt-monitoring-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture
        .provider_session
        .emit(watch_started("monitor-1", "tail -f server.log"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let monitoring = session_where(&fixture, "the Turn settles", |snapshot| {
        first_turn_settled(snapshot) && snapshot.watches.len() == 2
    })
    .await;
    assert!(monitoring.session.monitoring_since.is_some());
    assert_eq!(
        described(&monitoring),
        ["cargo test", "tail -f server.log"],
        "a reader viewing the Session is told what it waits on, earliest first"
    );

    let (response, ()) = tokio::join!(
        interrupt(
            &fixture.client,
            fixture.server.descriptor(),
            fixture.session_id
        ),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_watches_stop(),
            )
            .await
            .expect("the interrupt asks the Provider to stop the Watches");
            assert_eq!(
                stop.watches(),
                ["monitor-1", "task-tests"],
                "the stop names every live Watch"
            );
            stop.succeed();
        }
    );
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NO_CONTENT,
        "stopping Watches is stopping work, not withdrawing a Prompt"
    );
    assert!(
        fixture.provider_session.try_next_watches_stop().is_none(),
        "the Watches are stopped once"
    );

    // The Provider reports each stopped Watch settling, waking nothing.
    fixture.provider_session.emit(watch_stopped("task-tests"));
    fixture.provider_session.emit(watch_stopped("monitor-1"));
    let idle = session_where(&fixture, "the stopped Watches settle", |snapshot| {
        snapshot.session.monitoring_since.is_none()
    })
    .await;
    assert_eq!(idle.session.working_since, None);
    assert!(idle.watches.is_empty(), "{:?}", idle.watches);
    assert_eq!(
        idle.turns.len(),
        1,
        "stopping Watches settles no Turn and begins none"
    );
    assert_eq!(
        idle.turns[0].status,
        TurnStatus::Completed,
        "the Turn before the Watches keeps the settle it had"
    );
    assert_eq!(
        idle.prompts, monitoring.prompts,
        "no Prompt is withdrawn by stopping Watches"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subagents_watch_rolls_up_to_what_its_parent_waits_on_and_the_parents_interrupt_stops_it()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "interrupt-subtree-watches-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        });
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            watch_started("child-monitor", "Watch the deploy log"),
        )
        .await;
    fixture
        .provider_session
        .emit(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        });
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let monitoring = session_where(&fixture, "the whole tree stops Working", |snapshot| {
        first_turn_settled(snapshot) && snapshot.session.monitoring_since.is_some()
    })
    .await;
    assert_eq!(
        described(&monitoring),
        ["Watch the deploy log"],
        "a Watch a Subagent left running is what its parent waits on too"
    );

    let (response, ()) = tokio::join!(
        interrupt(
            &fixture.client,
            fixture.server.descriptor(),
            fixture.session_id
        ),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_watches_stop(),
            )
            .await
            .expect("the interrupt asks the Provider to stop the Watches below the Session");
            assert_eq!(stop.watches(), ["child-monitor"]);
            stop.succeed();
        }
    );
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("task-1")),
            watch_stopped("child-monitor"),
        )
        .await;
    let idle = session_where(&fixture, "the Subagent's Watch settles", |snapshot| {
        snapshot.session.monitoring_since.is_none()
    })
    .await;
    assert!(idle.watches.is_empty());
    assert_eq!(idle.turns.len(), 1);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_session_whose_subagents_outlived_its_turn_stops_its_watches_as_well() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "interrupt-watches-and-subagents-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
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
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let delegated = session_where(&fixture, "the Turn settles", first_turn_settled).await;
    assert!(delegated.session.working_since.is_some());

    let (response, ()) =
        tokio::join!(
            interrupt(
                &fixture.client,
                fixture.server.descriptor(),
                fixture.session_id
            ),
            async {
                let stop = timeout(
                    PROGRESS_DEADLINE,
                    fixture.provider_session.next_watches_stop(),
                )
                .await
                .expect("the interrupt stops the Watches");
                assert_eq!(stop.watches(), ["task-tests"]);
                stop.succeed();
                timeout(
            PROGRESS_DEADLINE,
            fixture.provider_session.next_subagents_stop(),
        )
        .await
        .expect("and the Subagents alike, since interrupting a top-level Session stops everything")
        .succeed();
            }
        );
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_watch_stop_the_provider_refuses_leaves_the_session_monitoring() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "refused-watch-stop-test").await;
    fixture
        .provider_session
        .emit(watch_started("task-tests", "cargo test"));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    session_where(&fixture, "the Turn settles into Monitoring", |snapshot| {
        first_turn_settled(snapshot) && snapshot.session.monitoring_since.is_some()
    })
    .await;

    let (response, ()) = tokio::join!(
        interrupt(
            &fixture.client,
            fixture.server.descriptor(),
            fixture.session_id
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_watches_stop(),
            )
            .await
            .expect("the interrupt asks the Provider to stop the Watch")
            .fail("the task is not ours to stop");
        }
    );
    assert_eq!(
        response.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "a stop the Provider refused is reported, not passed off as success"
    );
    let still = crate::support::read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        still.session.monitoring_since.is_some(),
        "a Watch the Provider would not stop is still live"
    );
    assert_eq!(described(&still), ["cargo test"]);
    assert_eq!(still.turns.len(), 1);
    fixture.server.shutdown().await.expect("shut down server");
}
