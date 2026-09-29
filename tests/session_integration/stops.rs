//! Stop affordances at the provider-neutral seam: interrupting a Session
//! reaches its working Subagents — with or without an active Turn — and a
//! single Subagent stops on its own where the Provider declares the
//! capability, refusing cleanly where it does not.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    server_support::next_catalog_change_matching,
    support::{
        WorkingTurn, open_catalog_stream_with_snapshot, read_session_until, the_subagent_row,
        working_turn,
    },
};
use axum::http::StatusCode;
use suru::{
    protocol::{
        Activity, ActivityStatus, RuntimeDescriptor, SessionCatalogChange, SessionError,
        SessionErrorCode, SessionId, SessionSnapshot, TurnStatus,
    },
    provider::{ProviderEvent, ProviderEventAttribution, ProviderSubagentId},
};
use tokio::time::timeout;

/// Spawns one Subagent into the fixture's working Turn — under `name`, so a
/// test can tell fan-out rows apart — and returns the child Session its row
/// names.
async fn spawn_subagent(fixture: &WorkingTurn, subagent: &ProviderSubagentId) -> SessionId {
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Subagent row opens",
        |snapshot| child_session_named(snapshot, subagent).is_some(),
    )
    .await;
    child_session_named(&parent, subagent).expect("the awaited row is present")
}

/// The child Session of the row a spawn under `subagent`'s well-known
/// description opened — rows are told apart by position, the order they
/// spawned in.
fn child_session_named(
    snapshot: &SessionSnapshot,
    subagent: &ProviderSubagentId,
) -> Option<SessionId> {
    let index = match subagent.as_str() {
        "task-1" => 0,
        "task-2" => 1,
        other => panic!("unexpected Subagent identity {other}"),
    };
    subagent_rows(snapshot).get(index).copied().map(|row| {
        let Activity::Subagent { session_id, .. } = row else {
            unreachable!()
        };
        *session_id
    })
}

/// The Subagent rows in `snapshot`, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .collect()
}

fn row_status(row: &Activity) -> ActivityStatus {
    let Activity::Subagent { status, .. } = row else {
        unreachable!()
    };
    *status
}

/// Asks the server to interrupt `session_id`, answering with the raw response
/// so refusals stay assertable.
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

#[tokio::test]
async fn interrupting_with_no_turn_active_stops_every_subagent_and_clears_working() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stop-all-subagents-test").await;
    let first = ProviderSubagentId::new("task-1");
    let second = ProviderSubagentId::new("task-2");
    let first_child = spawn_subagent(&fixture, &first).await;
    let second_child = spawn_subagent(&fixture, &second).await;
    let (_, mut catalog) = open_catalog_stream_with_snapshot(fixture.server.descriptor()).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
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
                fixture.provider_session.next_subagents_stop(),
            )
            .await
            .expect("the interrupt asks the Provider to stop its Subagents")
            .succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "both rows settle as stopped",
        |snapshot| {
            subagent_rows(snapshot)
                .iter()
                .all(|row| row_status(row) == ActivityStatus::Interrupted)
        },
    )
    .await;
    for row in subagent_rows(&parent) {
        let Activity::Subagent { duration_ms, .. } = row else {
            unreachable!()
        };
        assert!(
            duration_ms.is_some(),
            "a stop is a real settle, so the row states how long the delegation ran"
        );
    }
    for child_id in [first_child, second_child] {
        let child = read_session_until(
            &fixture.client,
            fixture.server.descriptor(),
            child_id,
            "the child's Turn settles with the stop",
            |snapshot| snapshot.turns[0].status != TurnStatus::Active,
        )
        .await;
        assert_eq!(child.turns[0].status, TurnStatus::Interrupted);
    }
    // Each settle may re-anchor the reading on the Subagents still left, so
    // the stream is read to the change that matters: Working clearing once
    // the last one stopped.
    next_catalog_change_matching(&mut catalog, |change| match change {
        SessionCatalogChange::WorkingChanged {
            session_id,
            working_since: None,
        } if *session_id == fixture.session_id => true,
        SessionCatalogChange::WorkingChanged { .. }
        | SessionCatalogChange::StandingInputsChanged { .. } => false,
        other => panic!("only work and its Standing move here, got {other:?}"),
    })
    .await;

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stopping_one_subagent_names_it_to_the_provider_and_leaves_the_rest_working() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stop-one-subagent-test").await;
    let first = ProviderSubagentId::new("task-1");
    let second = ProviderSubagentId::new("task-2");
    let first_child = spawn_subagent(&fixture, &first).await;
    spawn_subagent(&fixture, &second).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    // Interrupting the Subagent's own Session is the Picker row's stop.
    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), first_child),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider");
            assert_eq!(
                stop.subagent(),
                "task-1",
                "the stop names the Provider's own identity for the Subagent"
            );
            stop.succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the stopped row settles",
        |snapshot| {
            subagent_rows(snapshot)
                .first()
                .is_some_and(|row| row_status(row) == ActivityStatus::Interrupted)
        },
    )
    .await;
    assert_eq!(
        row_status(subagent_rows(&parent)[1]),
        ActivityStatus::Active,
        "the other Subagent works on: the stop reached one delegation, not the Session"
    );
    let stopped_child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        first_child,
        "the stopped child's Turn settles",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(stopped_child.turns[0].status, TurnStatus::Interrupted);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stopping_a_subagent_stops_whatever_it_delegated_in_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stop-subagent-subtree-test").await;
    let outer = ProviderSubagentId::new("task-1");
    let inner = ProviderSubagentId::new("task-inner");
    let outer_child = spawn_subagent(&fixture, &outer).await;
    // The nested spawn rides the outer Subagent's attribution, which is what
    // hangs the grandchild under the child.
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(outer.clone()),
            ProviderEvent::SubagentStarted {
                subagent_id: inner.clone(),
                name: "Scout".to_owned(),
                description: "Map the callers".to_owned(),
                delegation: None,
            },
        )
        .await;
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        outer_child,
        "the nested row opens in the child's own Transcript",
        |snapshot| !subagent_rows(snapshot).is_empty(),
    )
    .await;
    let Activity::Subagent {
        session_id: grandchild_id,
        ..
    } = subagent_rows(&child)[0]
    else {
        unreachable!()
    };
    let grandchild_id = *grandchild_id;

    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), outer_child),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider");
            assert_eq!(stop.subagent(), "task-1");
            stop.succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        outer_child,
        "the child settles with the nested row stopped",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        row_status(subagent_rows(&child)[0]),
        ActivityStatus::Interrupted,
        "stopping a delegation stops what it delegated in turn"
    );
    let grandchild = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        grandchild_id,
        "the grandchild's Turn settles with its spawner's stop",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(grandchild.turns[0].status, TurnStatus::Interrupted);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_provider_without_the_capability_refuses_the_per_subagent_stop() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "subagent-stop-unsupported-test").await;
    fixture.runtime.withdraw_subagent_stop();
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;

    let refused = interrupt(&fixture.client, fixture.server.descriptor(), child_id).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        refused
            .json::<SessionError>()
            .await
            .expect("decode the refusal")
            .code,
        SessionErrorCode::SubagentStopUnsupported
    );
    assert!(
        fixture.provider_session.try_next_subagent_stop().is_none(),
        "the refusal never reaches the Provider"
    );
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent stays readable",
        |_| true,
    )
    .await;
    assert_eq!(
        row_status(the_subagent_row(&parent)),
        ActivityStatus::Active,
        "a refused stop stops nothing"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_turn_settles_its_subagents_and_discards_their_late_echoes() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "interrupt-turn-subagents-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;

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

    // The Provider stopped the Turn's background work with the loop, so the
    // row settles on the acknowledgement rather than waiting on a settle the
    // ended loop may never deliver.
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the row settles as stopped on the acknowledgement",
        |snapshot| row_status(the_subagent_row(snapshot)) == ActivityStatus::Interrupted,
    )
    .await;
    assert_eq!(
        parent.turns[0].status,
        TurnStatus::Active,
        "the Turn itself still settles on the Provider's own boundary"
    );

    // Whatever account of the stopped Subagent the Provider still had in
    // flight arrives as a late echo to discard, never an invalid event.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: suru::provider::ProviderSubagentStatus::Completed,
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;

    let settled = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the interrupted Turn settles",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Interrupted,
        "the late echo neither fails the Session nor re-settles the row"
    );
    assert_eq!(
        row_status(the_subagent_row(&settled)),
        ActivityStatus::Interrupted
    );
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the child settles with the interrupt",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_delegation_reported_for_a_stopped_subagent_is_a_late_echo_to_discard() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stopped-subagent-delegation-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;
    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), child_id),
        async {
            timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider")
            .succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the stopped row settles",
        |snapshot| row_status(the_subagent_row(snapshot)) == ActivityStatus::Interrupted,
    )
    .await;

    // The Delegation the stopped Subagent's first input carried can still
    // trail the stop.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentDelegated {
            subagent_id: subagent,
            delegation: "Review the diff".to_owned(),
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Turn settles",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(
        parent.turns[0].status,
        TurnStatus::Completed,
        "the late echo does not fail the Turn"
    );
    let Activity::Subagent { description, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    assert_eq!(description, "Map the provider seams", "nor revises the row");
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the stopped child is readable",
        |_| true,
    )
    .await;
    assert!(
        child.messages.is_empty(),
        "a Delegation that trails the stop was never delivered, and stands nowhere"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_continuation_stops_the_subagents_and_settles_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "interrupt-continuation-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    // Late output owed to the Subagent begins a Continuation, which is what
    // the interrupt then finds active.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::AgentMessageStarted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Continuation begins",
        |snapshot| snapshot.turns.len() == 2,
    )
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
                fixture.provider_session.next_subagents_stop(),
            )
            .await
            .expect("interrupting a Continuation stops the Subagents that own it")
            .succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Continuation settles with the stop",
        |snapshot| snapshot.turns[1].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(
        parent.turns[1].status,
        TurnStatus::Interrupted,
        "no Provider loop runs a Continuation, so the interrupt settles it directly"
    );
    assert_eq!(parent.turns[1].prompt_id, None);
    assert_eq!(
        row_status(the_subagent_row(&parent)),
        ActivityStatus::Interrupted
    );
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the child settles with the stop",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_with_nothing_running_reports_nothing_to_interrupt() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "nothing-to-interrupt-test").await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let refused = interrupt(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        refused
            .json::<SessionError>()
            .await
            .expect("decode the refusal")
            .code,
        SessionErrorCode::NothingToInterrupt
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_stop_the_provider_refuses_leaves_the_subagent_running_and_reports_why() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "subagent-stop-refused-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;

    let (refused, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), child_id),
        async {
            timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider")
            .fail("fixture refused the stop");
        }
    );
    assert_eq!(refused.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        refused
            .json::<SessionError>()
            .await
            .expect("decode the refusal")
            .code,
        SessionErrorCode::InterruptionFailed
    );

    // One Subagent the Provider would not stop is no reason to tear the
    // Session down: the row works on, and the Provider's own settle still
    // lands on it later.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: suru::provider::ProviderSubagentStatus::Completed,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Provider's own settle still closes the row",
        |snapshot| row_status(the_subagent_row(snapshot)) == ActivityStatus::Completed,
    )
    .await;
    assert_eq!(
        row_status(the_subagent_row(&parent)),
        ActivityStatus::Completed
    );
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the child settles on the Provider's own terms",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stopping_a_resumed_subagent_settles_only_its_resumed_stretch() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stop-resumed-subagent-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child = spawn_subagent(&fixture, &subagent).await;
    for event in [
        ProviderEvent::SubagentCompleted {
            subagent_id: subagent.clone(),
            status: suru::provider::ProviderSubagentStatus::Completed,
        },
        ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the tests too".to_owned(),
            delegation: None,
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child,
        "the resume begins the child's second Turn",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;

    // The resumed Subagent's Session is what its row and the Picker stop.
    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), child),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider");
            assert_eq!(
                stop.subagent(),
                "task-1",
                "the stop names the Subagent by the identity its resume carried"
            );
            stop.succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the resume row settles as stopped",
        |snapshot| {
            subagent_rows(snapshot)
                .get(1)
                .is_some_and(|row| row_status(row) == ActivityStatus::Interrupted)
        },
    )
    .await;
    assert_eq!(
        row_status(subagent_rows(&parent)[0]),
        ActivityStatus::Completed,
        "the spawn's row stays as it settled"
    );
    let stopped = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child,
        "the resumed Turn settles",
        |snapshot| snapshot.turns[1].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(stopped.turns[0].status, TurnStatus::Completed);
    assert_eq!(stopped.turns[1].status, TurnStatus::Interrupted);
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "nothing is left Working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stopping_a_subagent_leaves_working_a_subagent_it_only_resumed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stop-resumer-test").await;
    let reviewer = ProviderSubagentId::new("task-1");
    let writer = ProviderSubagentId::new("task-2");
    let reviewer_child = spawn_subagent(&fixture, &reviewer).await;
    let writer_child = spawn_subagent(&fixture, &writer).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: writer.clone(),
            status: suru::provider::ProviderSubagentStatus::Completed,
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(reviewer.clone()),
            ProviderEvent::SubagentResumed {
                subagent_id: writer,
                name: "Explore".to_owned(),
                description: "Tighten the notes".to_owned(),
                delegation: None,
            },
        )
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        reviewer_child,
        "the reviewer holds the row of the resume it sent",
        |snapshot| subagent_rows(snapshot).len() == 1,
    )
    .await;

    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), reviewer_child),
        async {
            let stop = timeout(
                PROGRESS_DEADLINE,
                fixture.provider_session.next_subagent_stop(),
            )
            .await
            .expect("the stop reaches the Provider");
            assert_eq!(stop.subagent(), "task-1");
            stop.succeed();
        }
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let reviewer_session = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        reviewer_child,
        "the stopped reviewer's Turn settles",
        |snapshot| snapshot.turns[0].status == TurnStatus::Interrupted,
    )
    .await;
    assert_eq!(
        row_status(subagent_rows(&reviewer_session)[0]),
        ActivityStatus::Active,
        "the writer does not stand below the reviewer, so the reviewer's stop leaves it working"
    );
    let writer_session = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        writer_child,
        "the writer's resumed Turn is readable",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    assert_eq!(writer_session.turns[1].status, TurnStatus::Active);
    assert!(
        fixture.provider_session.try_next_subagent_stop().is_none(),
        "one stop went out, for the reviewer alone"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}
