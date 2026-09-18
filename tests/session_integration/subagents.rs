//! Subagents at the provider-neutral seam: a spawn opens a child Session and a
//! Subagent row in the spawner's Transcript, attributed content fills the child
//! and never the parent, and a settle closes the row and the child's Turn.

use crate::{
    provider_support::ControlledProvider,
    support::{
        open_catalog_stream_with_snapshot, read_session, read_session_at_least_revision,
        read_session_until, the_subagent_row, working_turn,
    },
};
use axum::http::StatusCode;
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, MessageRole, ModelId,
        PromptDelivery, PromptId, SessionCatalogChange, SessionChange, SessionError,
        SessionErrorCode, SessionListItem, SessionRevision, SessionSnapshot, SessionStatus,
        TurnStatus, ViewSessionOperationId, ViewSessionRequest,
    },
    provider::{
        ProviderActivityId, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderSubagentStatus,
    },
    server::{self, ServerConfig},
};

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
