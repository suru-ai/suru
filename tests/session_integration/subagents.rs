//! Subagents at the provider-neutral seam: a spawn opens a child Session and a
//! Subagent row in the spawner's Transcript, attributed content fills the child
//! and never the parent, and a settle closes the row and the child's Turn.

use crate::{
    provider_support::ControlledProvider,
    server_support::PROGRESS_DEADLINE,
    support::{
        WorkingTurn, open_catalog_stream_with_snapshot, read_session,
        read_session_at_least_revision, read_session_until, the_subagent_row, working_turn,
    },
};
use axum::http::StatusCode;
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, Delegator, InitialPrompt, MessageRole,
        ModelId, PromptDelivery, PromptId, SessionCatalogChange, SessionChange, SessionError,
        SessionErrorCode, SessionId, SessionListItem, SessionRevision, SessionSnapshot,
        SessionStatus, TranscriptItem, TurnId, TurnStatus, ViewSessionOperationId,
        ViewSessionRequest,
    },
    provider::{
        ProviderActivityId, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderSubagentStatus,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

#[tokio::test]
async fn a_subagents_confirmed_model_updates_its_session_and_parent_projection() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-model-test").await;
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
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    let child_id = *child_id;
    let parent_selection = parent.session.agent_selection.clone();
    let unknown = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(unknown.session.agent_selection, None);
    assert_eq!(unknown.turns[0].agent, None);

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentModelChanged {
            subagent_id: subagent,
            model: ModelId::new("child-model"),
        })
        .await;

    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the child Model is confirmed",
        |snapshot| {
            snapshot.turns[0]
                .agent
                .as_ref()
                .is_some_and(|agent| agent.selection.model.as_str() == "child-model")
        },
    )
    .await;
    assert_eq!(
        child.session.agent_selection, None,
        "observation is not selection"
    );

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent projection carries the child Model",
        |snapshot| {
            matches!(
                the_subagent_row(snapshot),
                Activity::Subagent { model: Some(model), .. } if model.as_str() == "child-model"
            )
        },
    )
    .await;
    assert_eq!(
        parent.session.agent_selection, parent_selection,
        "child evidence never changes the parent's Agent Selection"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_spawn_opens_a_subagent_row_in_the_parent_and_a_child_session_with_a_parent_link() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-spawn-test").await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;

    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    let Activity::Subagent {
        status,
        name,
        description,
        session_id: child_id,
        duration_ms,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(name, "Explore");
    assert_eq!(description, "Map the provider seams");
    assert_eq!(
        *duration_ms, None,
        "a working Subagent has no duration to state yet"
    );

    let child = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{child_id}",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("read child Session")
        .error_for_status()
        .expect("a child Session is reachable by its identity")
        .json::<SessionSnapshot>()
        .await
        .expect("decode child Session");
    assert_eq!(
        child.session.parent,
        Some(fixture.session_id),
        "the child names the Session whose Turn spawned it"
    );
    assert_eq!(
        child.session.workspace, parent.session.workspace,
        "the child works where its parent works"
    );
    assert!(
        child.prompts.is_empty(),
        "no Prompt made the child, so it holds none"
    );
    assert_eq!(child.turns.len(), 1, "the spawn opens the child's one Turn");
    assert_eq!(child.turns[0].status, TurnStatus::Active);
    assert_eq!(
        child.turns[0].prompt_id, None,
        "a Subagent's Turn begins without a Prompt"
    );
    assert!(child.messages.is_empty());
    assert!(child.activities.is_empty());

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn viewing_a_subagent_session_is_refused_without_changing_its_parent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-viewed-test").await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("viewed-child"),
            name: "Reader".to_owned(),
            description: "Inspect the child".to_owned(),
            delegation: None,
        })
        .await;
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };

    let response = fixture
        .client
        .post(format!(
            "{}/v1/sessions/{child_id}/viewed",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&ViewSessionRequest {
            operation_id: ViewSessionOperationId::new(),
        })
        .send()
        .await
        .expect("report the Subagent Session viewed");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the refusal")
            .code,
        SessionErrorCode::SubagentSession
    );
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
    assert_eq!(
        listed[0]
            .readable()
            .expect("the listed parent is readable")
            .standing_inputs
            .viewed_at,
        None,
        "a child view never changes the parent's reading"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn subagent_attributed_content_fills_the_child_session_and_never_the_parent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-attribution-test").await;
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
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
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

    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Reading the seams.".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("child-command"),
            command: "cargo tree".to_owned(),
            cwd: None,
        },
    ] {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }

    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(5),
    )
    .await;
    assert_eq!(child.messages.len(), 1);
    assert_eq!(child.messages[0].role, MessageRole::Agent);
    assert_eq!(child.messages[0].content, "Reading the seams.");
    assert_eq!(child.messages[0].turn_id, child.turns[0].id);
    assert_eq!(child.activities.len(), 1);
    assert!(matches!(
        &child.activities[0],
        Activity::Command { command, .. } if command == "cargo tree"
    ));

    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    assert_eq!(
        parent.revision,
        SessionRevision(4),
        "nothing of the Subagent's work commits to the parent"
    );
    assert_eq!(parent.messages.len(), 1, "only the user's own Prompt");
    assert_eq!(
        parent.activities.len(),
        1,
        "the Subagent row is all the parent's Transcript carries of it"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_settle_closes_the_row_with_its_outcome_and_duration_and_settles_the_childs_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-settle-test").await;
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
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
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

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;

    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(5),
    )
    .await;
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert!(
        duration_ms.is_some(),
        "a settle the Provider reported carries how long the Subagent worked"
    );

    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(2),
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert!(child.turns[0].settled_at.is_some());

    // With every Subagent settled, the Provider's own turn boundary passes.
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(6),
    )
    .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_settle_fails_the_row_and_the_childs_turn_says_why() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-failed-settle-test").await;
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
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
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

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Failed,
        })
        .await;

    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(5),
    )
    .await;
    let Activity::Subagent { status, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Failed);

    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(2),
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert!(
        child.activities.iter().any(
            |activity| matches!(activity, Activity::Error { text, .. } if text.contains("Subagent"))
        ),
        "the child's Transcript states the failure: {:?}",
        child.activities
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn child_sessions_join_no_listing_and_ride_no_catalog_stream() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-listing-test").await;
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
    assert_eq!(
        listed.iter().map(SessionListItem::id).collect::<Vec<_>>(),
        vec![fixture.session_id],
        "the child joins no Session listing"
    );

    let (snapshot_ids, mut catalog) =
        open_catalog_stream_with_snapshot(fixture.server.descriptor()).await;
    assert_eq!(
        snapshot_ids,
        vec![fixture.session_id],
        "the catalog stream's own snapshot carries no child either"
    );

    // Drive commits into the child — content, then its settle — and then the
    // parent Turn's own outcome. That parent's Standing is the first change
    // announced, which proves the child's commits rode no catalog stream.
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::AgentMessageStarted,
        )
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::AgentMessageCompleted,
        )
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    assert!(matches!(
        crate::server_support::next_catalog_change(&mut catalog).await,
        SessionCatalogChange::StandingInputsChanged { session_id, .. }
            if session_id == fixture.session_id
    ));
    assert_eq!(
        crate::server_support::next_catalog_change(&mut catalog).await,
        SessionCatalogChange::WorkingChanged {
            session_id: fixture.session_id,
            working_since: None,
        },
        "the parent Turn's settle then clears the listed root's Working reading"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn working_duration_stays_continuous_when_only_subagents_remain() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-working-duration-test").await;
    let before_spawn = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(3),
    )
    .await;
    let turn_started_at = before_spawn.turns[0]
        .started_at
        .expect("the parent Turn knows when it began");
    // Working began when the Prompt that owed this Turn was admitted, which
    // the Turn starting continues rather than restarts (ADR 0024).
    let started_at = before_spawn
        .working_since()
        .expect("the Session is Working");
    assert!(started_at < turn_started_at);

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let waiting = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent Turn settles while its Subagent keeps Working",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(
        waiting.working_since(),
        Some(started_at),
        "the open Session keeps the beginning of its uninterrupted Working interval"
    );

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
    let summary = listed
        .iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == fixture.session_id)
        })
        .expect("the parent Session remains listed");
    assert_eq!(
        summary.session.working_since,
        Some(started_at),
        "the Sidebar and open Session share one continuous Working clock"
    );
    assert_eq!(
        summary
            .standing_inputs
            .latest_turn
            .expect("the settled parent Turn is the latest Turn")
            .status,
        TurnStatus::Completed,
        "the outcome is present but Working still takes precedence while the Subagent lives"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

/// A Subagent that outlives its Turn is still the Provider's work, and the
/// Provider does not survive the server stopping. The stop settles it the way
/// a lost Provider connection does — before the storage writer closes, so the
/// settlement is what the next process reads rather than a Turn it must
/// settle for itself — and the restarted server reports the parent idle
/// instead of waiting on Subagents no process is running (ADR 0029).
#[tokio::test]
async fn a_stop_settles_the_subagents_no_process_will_finish() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let instance = "subagent-stop-settles-test";
    let config = ServerConfig::new(state_dir.path(), instance).expect("configure server");
    let fixture = working_turn(state_dir.path(), instance).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let waiting = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent Turn settles while its Subagent keeps Working",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert!(waiting.working_since().is_some());
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&waiting)
    else {
        unreachable!()
    };
    let child_id = *child_id;

    // The Provider connection is alive when the server stops: the stop itself
    // is what ends the Subagent's work.
    fixture.server.shutdown().await.expect("shut down server");
    drop(fixture.provider_session);

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("respawn server");
    let listed = reqwest::Client::new()
        .get(format!("{}/v1/sessions", restarted.descriptor().base_url))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session listing");
    let summary = listed
        .iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == fixture.session_id)
        })
        .expect("the parent Session remains listed");
    assert_eq!(
        summary.session.working_since, None,
        "the listing reads no Working from work no process is doing, before anyone opens it"
    );

    let restored_child = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(restored_child.turns.len(), 1);
    assert_eq!(restored_child.turns[0].status, TurnStatus::Failed);
    assert!(
        restored_child.turns[0].settled_at.is_some(),
        "the settled Turn says when it settled"
    );
    assert_eq!(restored_child.session.status, SessionStatus::Idle);
    assert_eq!(restored_child.working_since(), None);
    let reasons = restored_child
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Error { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reasons,
        vec![
            "Provider execution failed: the Provider Session ended before the Subagent's work completed."
        ],
        "the stopping server settled the Subagent itself, so the next process had nothing to settle"
    );

    let restored_parent = read_session(restarted.descriptor(), fixture.session_id).await;
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = the_subagent_row(&restored_parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        *duration_ms, None,
        "a Subagent the Provider never reported settling has no duration"
    );
    assert_eq!(restored_parent.working_since(), None);
    assert_eq!(restored_parent.session.status, SessionStatus::Idle);
    restarted.shutdown().await.expect("stop restarted server");
}

/// A history that reaches the next start with Turns still open — a crash, a
/// kill, or a database written before stops settled their own work — is
/// settled by that start: the listing reads nothing as Working or Active
/// before any tree is opened, opening the tree settles the Turns durably at
/// the moment they last showed work, and the parent's Subagent row settles
/// with the child (ADR 0029).
#[tokio::test]
async fn a_restart_settles_the_turns_the_last_process_left_open() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let instance = "subagent-restart-repair-test";
    let config = ServerConfig::new(state_dir.path(), instance).expect("configure server");
    let fixture = working_turn(state_dir.path(), instance).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let spawned = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let Activity::Subagent {
        id: row_id,
        session_id: child_id,
        ..
    } = the_subagent_row(&spawned)
    else {
        unreachable!()
    };
    let (row_id, child_id) = (*row_id, *child_id);
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    // Put the history back the way a process that never settled its work
    // would have left it: both Turns open, the Subagent row still live.
    {
        use diesel::{Connection, RunQueryDsl, SqliteConnection};
        let mut database =
            SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
                .unwrap();
        diesel::sql_query(
            "UPDATE turns SET payload = json_set(payload, '$.status', 'active', '$.settled_at', json('null'))",
        )
        .execute(&mut database)
        .unwrap();
        diesel::sql_query(format!(
            "UPDATE activities SET payload = json_set(payload, '$.status', 'active', '$.duration_ms', json('null')) WHERE id = '{row_id}'"
        ))
        .execute(&mut database)
        .unwrap();
    }

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config.clone(), runtime)
        .await
        .expect("respawn server");
    let listed = reqwest::Client::new()
        .get(format!("{}/v1/sessions", restarted.descriptor().base_url))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session listing");
    let summary = listed
        .iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == fixture.session_id)
        })
        .expect("the parent Session remains listed");
    assert_eq!(
        summary.session.working_since, None,
        "before any tree is opened, the listing reads no Working from Turns no process is running"
    );
    assert_eq!(summary.session.status, SessionStatus::Idle);

    let child = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert_eq!(
        child.turns[0].settled_at, child.turns[0].started_at,
        "a Turn that never showed output settles where it began"
    );
    assert_eq!(child.session.status, SessionStatus::Idle);
    assert!(child.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text == "The server stopped before this Subagent finished."
    )));
    let parent = read_session(restarted.descriptor(), fixture.session_id).await;
    assert_eq!(parent.turns[0].status, TurnStatus::Failed);
    assert!(parent.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text == "The server stopped before this Turn finished."
    )));
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!((*status, *duration_ms), (ActivityStatus::Failed, None));
    assert_eq!(parent.working_since(), None);
    assert_eq!(parent.session.status, SessionStatus::Idle);
    restarted.shutdown().await.expect("stop restarted server");

    // The repair is durable: the next process reads it back rather than
    // deciding it again.
    let (runtime, _provider) = ControlledProvider::new();
    let again = server::spawn_with_provider(config, runtime)
        .await
        .expect("respawn server once more");
    assert_eq!(
        read_session(again.descriptor(), child_id).await.revision,
        child.revision
    );
    assert_eq!(
        read_session(again.descriptor(), fixture.session_id)
            .await
            .revision,
        parent.revision
    );
    again.shutdown().await.expect("stop final server");
}

#[tokio::test]
async fn callers_cannot_override_the_server_derived_working_clock() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "working-clock-injection-test").await;
    let before = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Turn is Working",
        |snapshot| snapshot.working_since().is_some(),
    )
    .await;
    let canonical = before.working_since();

    let update = fixture
        .server
        .session_event_sink()
        .publish(
            fixture.session_id,
            vec![SessionChange::SessionWorkingChanged {
                working_since: None,
            }],
        )
        .await
        .expect("publish an attempted Working override");
    assert!(
        update.changes.is_empty(),
        "the ordinary commit boundary strips caller-supplied Working state"
    );
    let after = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        update.revision,
    )
    .await;
    assert_eq!(after.working_since(), canonical);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_child_session_refuses_prompts() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-prompt-refusal-test").await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };

    let refused = fixture
        .client
        .post(format!(
            "{}/v1/sessions/{child_id}/prompts",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Steer the Subagent".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("submit Prompt to child Session");
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let error = refused
        .json::<SessionError>()
        .await
        .expect("decode refusal");
    assert_eq!(error.code, SessionErrorCode::SubagentSession);

    let child = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{child_id}",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("read child Session")
        .error_for_status()
        .expect("child Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode child Session");
    assert!(
        child.prompts.is_empty(),
        "the refused Prompt left no mark on the child"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_nested_spawn_records_its_row_in_the_childs_own_transcript_one_level_down() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-nesting-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let nested = ProviderSubagentId::new("task-2");

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
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

    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::SubagentStarted {
                subagent_id: nested.clone(),
                name: "Plan".to_owned(),
                description: "Weigh the seam options".to_owned(),
                delegation: None,
            },
        )
        .await;

    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(2),
    )
    .await;
    let Activity::Subagent {
        status,
        name,
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(name, "Plan");
    let grandchild_id = *grandchild_id;
    let parent_after = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    assert_eq!(
        parent_after.activities.len(),
        1,
        "a nested spawn shows nothing new in the grandparent"
    );

    let grandchild = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        grandchild_id,
        SessionRevision(1),
    )
    .await;
    assert_eq!(
        grandchild.session.parent,
        Some(child_id),
        "a Subagent's Subagent is the child's child"
    );

    // The grandchild's stream lands one level down too, and its settle closes
    // the row in the child's Transcript.
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(nested.clone()),
            ProviderEvent::AgentMessageStarted,
        )
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(nested),
            ProviderEvent::AgentMessageCompleted,
        )
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new("task-2"),
                status: ProviderSubagentStatus::Completed,
            },
        )
        .await;
    let grandchild = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        grandchild_id,
        SessionRevision(4),
    )
    .await;
    assert_eq!(grandchild.messages.len(), 1);
    assert_eq!(grandchild.turns[0].status, TurnStatus::Completed);
    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(3),
    )
    .await;
    let Activity::Subagent { status, .. } = the_subagent_row(&child) else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn deleting_a_session_deletes_its_subagent_subtree_for_good() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "subagent-delete-test").expect("configure server");
    let fixture = working_turn(state_dir.path(), "subagent-delete-test").await;
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
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new("task-2"),
                name: "Plan".to_owned(),
                description: "Weigh the seam options".to_owned(),
                delegation: None,
            },
        )
        .await;
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
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
    let child = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        SessionRevision(2),
    )
    .await;
    let Activity::Subagent {
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    let grandchild_id = *grandchild_id;

    let refused = fixture
        .client
        .delete(format!(
            "{}/v1/sessions/{}",
            fixture.server.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("delete parent Session");
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let error = refused
        .json::<SessionError>()
        .await
        .expect("decode Working deletion refusal");
    assert_eq!(error.code, SessionErrorCode::WorkingSession);
    for surviving in [fixture.session_id, child_id, grandchild_id] {
        let read = fixture
            .client
            .get(format!(
                "{}/v1/sessions/{surviving}",
                fixture.server.descriptor().base_url
            ))
            .bearer_auth(&fixture.server.descriptor().token)
            .send()
            .await
            .expect("read Session after refused deletion");
        assert_eq!(read.status(), StatusCode::OK);
    }

    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new("task-2"),
                status: ProviderSubagentStatus::Completed,
            },
        )
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the whole subtree stops Working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;

    let deleted = fixture
        .client
        .delete(format!(
            "{}/v1/sessions/{}",
            fixture.server.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("delete idle parent Session");
    assert!(
        deleted.status().is_success(),
        "deletion succeeds: {}",
        deleted.status()
    );
    for orphan in [fixture.session_id, child_id, grandchild_id] {
        let read = fixture
            .client
            .get(format!(
                "{}/v1/sessions/{orphan}",
                fixture.server.descriptor().base_url
            ))
            .bearer_auth(&fixture.server.descriptor().token)
            .send()
            .await
            .expect("read deleted Session");
        assert_eq!(
            read.status(),
            StatusCode::NOT_FOUND,
            "the whole subtree goes with the parent"
        );
    }

    // A restart proves the subtree left storage too, not just the live map.
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("respawn server");
    for orphan in [fixture.session_id, child_id, grandchild_id] {
        let read = fixture
            .client
            .get(format!(
                "{}/v1/sessions/{orphan}",
                restarted.descriptor().base_url
            ))
            .bearer_auth(&restarted.descriptor().token)
            .send()
            .await
            .expect("read deleted Session after restart");
        assert_eq!(read.status(), StatusCode::NOT_FOUND);
    }
    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_child_session_cannot_be_deleted_out_from_under_its_parents_row() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-child-delete-test").await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(4),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };

    let refused = fixture
        .client
        .delete(format!(
            "{}/v1/sessions/{child_id}",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("attempt to delete child Session");
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let error = refused
        .json::<SessionError>()
        .await
        .expect("decode refusal");
    assert_eq!(error.code, SessionErrorCode::SubagentSession);

    let child = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{child_id}",
            fixture.server.descriptor().base_url
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .send()
        .await
        .expect("read child Session")
        .error_for_status()
        .expect("the parent's row still reaches its child");
    drop(child);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restart_restores_child_sessions_and_the_rows_that_reach_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "subagent-restart-test").expect("configure server");
    let fixture = working_turn(state_dir.path(), "subagent-restart-test").await;
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
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::AgentMessageStarted,
        )
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::AgentMessageDelta {
                content: "Reading the seams.".to_owned(),
            },
        )
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::AgentMessageCompleted,
        )
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(6),
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

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("respawn server");

    let parent = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{}",
            restarted.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("read restored parent")
        .error_for_status()
        .expect("parent survives the restart")
        .json::<SessionSnapshot>()
        .await
        .expect("decode restored parent");
    let Activity::Subagent {
        status,
        name,
        description,
        session_id: restored_child_id,
        duration_ms,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(name, "Explore");
    assert_eq!(description, "Map the provider seams");
    assert_eq!(
        *restored_child_id, child_id,
        "the restored row still names the child it opened"
    );
    assert!(duration_ms.is_some());

    let child = fixture
        .client
        .get(format!(
            "{}/v1/sessions/{child_id}",
            restarted.descriptor().base_url
        ))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("read restored child")
        .error_for_status()
        .expect("child survives the restart")
        .json::<SessionSnapshot>()
        .await
        .expect("decode restored child");
    assert_eq!(child.session.parent, Some(fixture.session_id));
    assert_eq!(child.messages.len(), 1);
    assert_eq!(child.messages[0].content, "Reading the seams.");
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(child.turns[0].prompt_id, None);

    let listed = fixture
        .client
        .get(format!("{}/v1/sessions", restarted.descriptor().base_url))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("list restored Sessions")
        .error_for_status()
        .expect("listing succeeds after the restart")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode restored listing");
    assert_eq!(
        listed.iter().map(SessionListItem::id).collect::<Vec<_>>(),
        vec![fixture.session_id],
        "a restored child stays out of every listing"
    );

    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_provider_status_update_revises_what_the_row_says_the_subagent_is_doing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-update-test").await;
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
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentUpdated {
            subagent_id: subagent,
            description: "Reading the orchestration actor".to_owned(),
        })
        .await;

    let parent = read_session_at_least_revision(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        SessionRevision(5),
    )
    .await;
    let Activity::Subagent {
        status,
        description,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(description, "Reading the orchestration actor");

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

/// The Subagent rows in `snapshot`, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .collect()
}

/// Emits `events` attributed to `subagent`, each observed before the next.
async fn emit_for(
    fixture: &WorkingTurn,
    subagent: &ProviderSubagentId,
    events: Vec<ProviderEvent>,
) {
    for event in events {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }
}

fn agent_message(content: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: content.to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ]
}

/// A Subagent spawned in the fixture's working Turn, its first stretch of work
/// done under `model` and settled, and the Session that is its own.
async fn spawned_and_settled(
    fixture: &WorkingTurn,
    subagent: &ProviderSubagentId,
    model: &str,
) -> SessionId {
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    emit_for(fixture, subagent, agent_message("Mapped the seams.")).await;
    for event in [
        ProviderEvent::SubagentModelChanged {
            subagent_id: subagent.clone(),
            model: ModelId::new(model),
        },
        ProviderEvent::SubagentCompleted {
            subagent_id: subagent.clone(),
            status: ProviderSubagentStatus::Completed,
        },
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Subagent's first stretch settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [Activity::Subagent {
                    status: ActivityStatus::Completed,
                    model: Some(_),
                    ..
                }]
            )
        },
    )
    .await;
    let Activity::Subagent { session_id, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    *session_id
}

#[tokio::test]
async fn a_resume_begins_the_next_turn_in_the_subagents_own_session_and_a_row_of_its_own() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-resume-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawned_and_settled(&fixture, &subagent, "first-model").await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the tests too".to_owned(),
            delegation: None,
        })
        .await;

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the resume adds a row of its own",
        |snapshot| subagent_rows(snapshot).len() == 2,
    )
    .await;
    let [
        Activity::Subagent {
            status: spawn_status,
            description: spawn_description,
            model: spawn_model,
            duration_ms: spawn_duration,
            ..
        },
        Activity::Subagent {
            turn_id,
            status,
            name,
            description,
            model,
            session_id,
            duration_ms,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        unreachable!()
    };
    assert_eq!(*spawn_status, ActivityStatus::Completed);
    assert_eq!(spawn_description, "Map the provider seams");
    assert_eq!(
        spawn_model.as_ref().map(ModelId::as_str),
        Some("first-model")
    );
    assert!(
        spawn_duration.is_some(),
        "the spawn's row stays exactly as it settled"
    );
    assert_eq!(*turn_id, parent.turns[0].id, "the delegating Turn holds it");
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(
        name, "Explore",
        "the row names the same agent its spawn did"
    );
    assert_eq!(description, "Map the tests too");
    assert_eq!(*model, None, "no Model evidence of this stretch yet");
    assert_eq!(*session_id, child_id, "the row leads into the one Session");
    assert_eq!(*duration_ms, None);

    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(
        child.title, "Map the provider seams",
        "the Title stays the spawn's"
    );
    let [first, second] = child.turns.as_slice() else {
        panic!("the resume begins a second Turn, got {:?}", child.turns);
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(second.status, TurnStatus::Active);
    assert_eq!(
        second.prompt_id, None,
        "a Delegation, not a Prompt, begins it"
    );
    assert_eq!(second.agent, None);

    emit_for(&fixture, &subagent, agent_message("Mapped the tests.")).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentModelChanged {
            subagent_id: subagent.clone(),
            model: ModelId::new("resumed-model"),
        })
        .await;
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the resumed stretch's Model evidence reaches its Turn",
        |snapshot| snapshot.turns[1].agent.is_some(),
    )
    .await;
    let models = child
        .turns
        .iter()
        .map(|turn| {
            turn.agent
                .as_ref()
                .map(|agent| agent.selection.model.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        models,
        [Some("first-model"), Some("resumed-model")],
        "each Turn carries its own stretch's Model evidence"
    );
    let resumed = child
        .messages
        .iter()
        .find(|message| message.content == "Mapped the tests.")
        .expect("the resumed stretch's Message reaches the child");
    assert_eq!(
        resumed.turn_id, child.turns[1].id,
        "the resumed work lands in the Turn the resume began"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the resume row settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [
                    _,
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                ]
            )
        },
    )
    .await;
    let [
        Activity::Subagent {
            model: spawn_model, ..
        },
        Activity::Subagent {
            model, duration_ms, ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        unreachable!()
    };
    assert_eq!(
        spawn_model.as_ref().map(ModelId::as_str),
        Some("first-model")
    );
    assert_eq!(model.as_ref().map(ModelId::as_str), Some("resumed-model"));
    assert!(
        duration_ms.is_some(),
        "the resume row times its own stretch"
    );
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the resumed Turn settles with its row",
        |snapshot| snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(child.turns.len(), 2);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_resume_after_its_delegating_turn_settled_keeps_the_parent_working_until_it_settles() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-resume-working-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawned_and_settled(&fixture, &subagent, "first-model").await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent settles with nothing left working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;

    // The Provider's loop wakes to deliver the outcome and resumes the agent
    // from the Continuation that begins.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the tests too".to_owned(),
            delegation: None,
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
        "the Continuation holding the resume settles",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    let [
        _,
        Activity::Subagent {
            turn_id, status, ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!("the resume adds its row, got {:?}", parent.activities);
    };
    assert_eq!(
        *turn_id, parent.turns[1].id,
        "the resume's row stands in the Continuation it began"
    );
    assert_eq!(*status, ActivityStatus::Active);
    assert!(
        parent.working_since().is_some(),
        "the resumed Subagent keeps its parent Working past every settled Turn (ADR 0015)"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent stops Working once the resumed stretch settles",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(child.turns[1].status, TurnStatus::Completed);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

/// A resume Suru cannot place — no Session it holds carries the identity, as
/// for a Subagent spawned before identities were stored — is recorded as a
/// new Subagent of its own rather than dropped or failing the Turn.
#[tokio::test]
async fn a_resume_naming_a_subagent_suru_holds_no_record_of_is_recorded_as_a_new_subagent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-unknown-resume-test").await;
    let subagent = ProviderSubagentId::new("never-spawned");

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            name: "general-purpose".to_owned(),
            description: "Carry on".to_owned(),
            delegation: Some("Pick up where you left off.".to_owned()),
        })
        .await;
    emit_for(&fixture, &subagent, agent_message("Carrying on.")).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the new Subagent's row settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [Activity::Subagent {
                    status: ActivityStatus::Completed,
                    ..
                }]
            )
        },
    )
    .await;
    assert_eq!(
        parent.turns[0].status,
        TurnStatus::Active,
        "a resume Suru cannot place does not fail the Turn that sent it"
    );
    let Activity::Subagent {
        turn_id,
        name,
        description,
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    assert_eq!(*turn_id, parent.turns[0].id);
    assert_eq!(name, "general-purpose");
    assert_eq!(description, "Carry on");

    let child = read_session(fixture.server.descriptor(), *child_id).await;
    assert_eq!(child.session.parent, Some(fixture.session_id));
    assert_eq!(child.title, "Carry on");
    let [turn] = child.turns.as_slice() else {
        panic!(
            "the new Subagent's Session opens with one Turn, got {:?}",
            child.turns
        );
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        delegations(&child),
        [(
            turn.id,
            Delegator {
                session_id: fixture.session_id,
                name: None,
            },
            "Pick up where you left off.",
        )],
        "the resume's Delegation opens the new Subagent's Turn, as a spawn's would"
    );
    let agent_messages = child
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Agent)
        .collect::<Vec<_>>();
    let [message] = agent_messages[..] else {
        panic!("the resumed work is visible in the new Subagent's Session");
    };
    assert_eq!(message.content, "Carrying on.");
    assert_eq!(message.turn_id, turn.id);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

/// The Delegations in `snapshot`, in Transcript order: each one's Turn, its
/// sender, and what it asked.
fn delegations(snapshot: &SessionSnapshot) -> Vec<(TurnId, Delegator, &str)> {
    snapshot
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Message { message_id } => snapshot
                .messages
                .iter()
                .find(|message| message.id == *message_id),
            TranscriptItem::Activity { .. } => None,
        })
        .filter_map(|message| {
            message
                .role
                .delegator()
                .map(|delegator| (message.turn_id, delegator.clone(), message.content.as_str()))
        })
        .collect()
}

/// Each Turn a Delegation begins opens with it, as a Message from the Agent
/// that delegated — the spawn's in the child's first Turn, the resume's in
/// the Turn the resume began — and it is stored like any other Message, so a
/// restarted server reads it back. A Subagent's own spawn names that
/// Subagent as the grandchild's delegating Agent.
#[tokio::test]
async fn each_turn_a_delegation_begins_opens_with_it_as_a_message_from_the_delegating_agent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let instance = "subagent-delegation-test";
    let config = ServerConfig::new(state_dir.path(), instance).expect("configure server");
    let fixture = working_turn(state_dir.path(), instance).await;
    let subagent = ProviderSubagentId::new("task-1");
    let grandchild = ProviderSubagentId::new("task-2");

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: Some("Map where each Provider plugs in.\nList the files.".to_owned()),
        })
        .await;
    emit_for(&fixture, &subagent, agent_message("Mapped the seams.")).await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::SubagentStarted {
                subagent_id: grandchild.clone(),
                name: "Scout".to_owned(),
                description: "Read the Claude seam".to_owned(),
                delegation: Some("Read src/provider/claude.rs.".to_owned()),
            },
        )
        .await;
    for event in [
        ProviderEvent::SubagentCompleted {
            subagent_id: grandchild,
            status: ProviderSubagentStatus::Completed,
        },
        ProviderEvent::SubagentCompleted {
            subagent_id: subagent.clone(),
            status: ProviderSubagentStatus::Completed,
        },
        ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the tests too".to_owned(),
            delegation: Some("Now map the tests.".to_owned()),
        },
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    emit_for(&fixture, &subagent, agent_message("Mapped the tests.")).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the resumed stretch settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [
                    _,
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                ]
            )
        },
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = subagent_rows(&parent)[0]
    else {
        unreachable!()
    };
    let child_id = *child_id;
    let child = read_session(fixture.server.descriptor(), child_id).await;
    let from_parent = Delegator {
        session_id: fixture.session_id,
        name: None,
    };
    assert_eq!(
        delegations(&child),
        [
            (
                child.turns[0].id,
                from_parent.clone(),
                "Map where each Provider plugs in.\nList the files."
            ),
            (child.turns[1].id, from_parent, "Now map the tests."),
        ],
        "the spawn's and the resume's Delegations each open the Turn they began, from the \
         parent's Agent"
    );
    let opening = |turn_id: TurnId| {
        child
            .transcript
            .iter()
            .find_map(|item| match item {
                TranscriptItem::Message { message_id } => child
                    .messages
                    .iter()
                    .find(|message| message.id == *message_id && message.turn_id == turn_id),
                TranscriptItem::Activity { .. } => None,
            })
            .expect("the Turn has a Message")
    };
    for turn in &child.turns {
        let opening = opening(turn.id);
        assert!(
            matches!(opening.role, MessageRole::Delegation(_)),
            "each Turn opens with its Delegation, ahead of the Subagent's own work: {opening:?}"
        );
        assert_eq!(opening.status, suru::protocol::MessageStatus::Completed);
        assert!(!opening.truncated);
    }
    let Some(Activity::Subagent {
        session_id: grandchild_id,
        ..
    }) = child
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        panic!("the Subagent's own spawn stands in its Transcript");
    };
    let grandchild_id = *grandchild_id;
    let grandchild = read_session(fixture.server.descriptor(), grandchild_id).await;
    assert_eq!(
        delegations(&grandchild),
        [(
            grandchild.turns[0].id,
            Delegator {
                session_id: child_id,
                name: Some("Explore".to_owned()),
            },
            "Read src/provider/claude.rs."
        )],
        "a Subagent's own spawn names that Subagent, by its Session and its name, as the \
         delegating Agent"
    );

    fixture.server.shutdown().await.expect("shut down server");
    drop(fixture.provider_session);
    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("respawn server");
    let restored = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(
        delegations(&restored),
        delegations(&child),
        "a Delegation is stored like any other Message"
    );
    let restored_grandchild = read_session(restarted.descriptor(), grandchild_id).await;
    assert_eq!(delegations(&restored_grandchild), delegations(&grandchild));
    restarted.shutdown().await.expect("stop restarted server");
}

/// Every Provider identity stored with a child Session, as the database holds
/// it: `(child Session, Provider, identity)`, in no particular order.
fn stored_subagent_identities(config: &ServerConfig) -> Vec<(String, String, String)> {
    use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::Text};

    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        session_id: String,
        #[diesel(sql_type = Text)]
        provider: String,
        #[diesel(sql_type = Text)]
        subagent_id: String,
    }

    let mut database =
        SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
            .expect("open the Session database");
    let mut rows = diesel::sql_query(
        "SELECT session_id, provider, subagent_id FROM provider_subagent_identities",
    )
    .load::<Row>(&mut database)
    .expect("read the stored Subagent identities")
    .into_iter()
    .map(|row| (row.session_id, row.provider, row.subagent_id))
    .collect::<Vec<_>>();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_subagents_identity_is_stored_with_its_session_and_deleted_along_with_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let instance = "subagent-identity-storage-test";
    let config = ServerConfig::new(state_dir.path(), instance).expect("configure server");
    let fixture = working_turn(state_dir.path(), instance).await;
    let subagent = ProviderSubagentId::new("task-1");
    let nested = ProviderSubagentId::new("task-2");
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent.clone()),
            ProviderEvent::SubagentStarted {
                subagent_id: nested.clone(),
                name: "Plan".to_owned(),
                description: "Weigh the seam options".to_owned(),
                delegation: None,
            },
        )
        .await;
    for settled in [nested, subagent] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
                subagent_id: settled,
                status: ProviderSubagentStatus::Completed,
            })
            .await;
    }
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    let child_id = *child_id;
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the nested Subagent's row settles in its spawner",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [Activity::Subagent {
                    status: ActivityStatus::Completed,
                    ..
                }]
            )
        },
    )
    .await;
    let Activity::Subagent {
        session_id: grandchild_id,
        ..
    } = the_subagent_row(&child)
    else {
        unreachable!()
    };
    let grandchild_id = *grandchild_id;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the whole tree stops Working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    let mut expected = vec![
        (
            child_id.to_string(),
            "controlled".to_owned(),
            "task-1".to_owned(),
        ),
        (
            grandchild_id.to_string(),
            "controlled".to_owned(),
            "task-2".to_owned(),
        ),
    ];
    expected.sort();
    assert_eq!(
        stored_subagent_identities(&config),
        expected,
        "each Subagent's identity is stored with its own Session, a nested one's included"
    );

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config.clone(), runtime)
        .await
        .expect("respawn server");
    let deleted = reqwest::Client::new()
        .delete(format!(
            "{}/v1/sessions/{}",
            restarted.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("delete the parent Session");
    assert!(
        deleted.status().is_success(),
        "deletion succeeds: {}",
        deleted.status()
    );
    restarted.shutdown().await.expect("shut down server");

    assert_eq!(
        stored_subagent_identities(&config),
        Vec::new(),
        "the identities go with the Sessions they were stored with"
    );
}

/// The resume a Provider sends after Suru restarted and carried its
/// conversation on: the Subagent's identity was stored with its Session at
/// the spawn, so the resume continues that Session as its next Turn rather
/// than opening another.
#[tokio::test]
async fn a_resume_after_a_restart_lands_in_the_subagents_original_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let instance = "subagent-restart-resume-test";
    let config = ServerConfig::new(state_dir.path(), instance).expect("configure server");
    let fixture = working_turn(state_dir.path(), instance).await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawned_and_settled(&fixture, &subagent, "first-model").await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the parent settles with nothing left working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, mut provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, runtime)
        .await
        .expect("respawn server");
    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            restarted.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&restarted.descriptor().token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Send the explorer back in".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("admit a Prompt after the restart")
        .error_for_status()
        .expect("the restored Session takes a Prompt");
    let mut provider_session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the Provider connection resumes")
        .succeed(provider_agent());
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the Prompt reaches the Provider")
        .succeed();

    provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentResumed {
            subagent_id: subagent.clone(),
            // The Provider's own account of the agent; the rows keep the name
            // its spawn gave it.
            name: "general-purpose".to_owned(),
            description: "Map the tests too".to_owned(),
            delegation: Some("Now map the tests too.".to_owned()),
        })
        .await;
    for event in agent_message("Mapped the tests.") {
        provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }
    for event in [
        ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        },
        ProviderEvent::TurnCompleted,
    ] {
        provider_session.emit_and_wait_until_observed(event).await;
    }

    let parent = read_session_until(
        &fixture.client,
        restarted.descriptor(),
        fixture.session_id,
        "the resume's row settles in the Turn after the restart",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [
                    _,
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                ]
            )
        },
    )
    .await;
    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            session_id: spawn_child,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            name,
            description,
            session_id: resume_child,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the spawn and the resume each stand as a row, got {:?}",
            parent.activities
        );
    };
    assert_eq!(*spawn_turn, parent.turns[0].id);
    assert_eq!(
        *resume_turn, parent.turns[1].id,
        "the resume row stands in the Turn that delegated it"
    );
    assert_eq!(*spawn_child, child_id);
    assert_eq!(
        *resume_child, child_id,
        "the resume leads into the Subagent's original Session, not a new one"
    );
    assert_eq!(
        name, "Explore",
        "the resume row names the agent its spawn did"
    );
    assert_eq!(description, "Map the tests too");

    let child = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(child.title, "Map the provider seams");
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the resume begins a second Turn in the original Session, got {:?}",
            child.turns
        );
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(second.status, TurnStatus::Completed);
    let resumed = child
        .messages
        .iter()
        .find(|message| message.content == "Mapped the tests.")
        .expect("the resumed work reaches the original Session");
    assert_eq!(resumed.turn_id, second.id);
    assert_eq!(
        delegations(&child),
        [(
            second.id,
            Delegator {
                session_id: fixture.session_id,
                name: None,
            },
            "Now map the tests too.",
        )],
        "the resume after the restart opens its Turn with its Delegation"
    );

    drop(provider_session);
    restarted.shutdown().await.expect("shut down server");
}

fn provider_agent() -> suru::protocol::AgentIdentity {
    suru::protocol::AgentIdentity {
        agent: suru::protocol::AgentId::new("controlled-agent"),
        selection: crate::support::controlled_selection("gpt-subagent", "high", "fast"),
    }
}
