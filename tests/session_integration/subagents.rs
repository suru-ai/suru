//! Subagents at the provider-neutral seam: a spawn opens a child Session and a
//! Subagent row in the spawner's Transcript, attributed content fills the child
//! and never the parent, and a settle closes the row and the child's Turn.

use crate::{
    provider_support::{ControlledProvider, ControlledProviderSession},
    support::{controlled_selection, read_session_at_least_revision},
};
use axum::http::StatusCode;
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity, CreateSessionRequest,
        InitialPrompt, MessageRole, PromptDelivery, PromptId, SessionCatalogChange, SessionError,
        SessionErrorCode, SessionListItem, SessionRevision, SessionSnapshot, TurnStatus, Workspace,
    },
    provider::{
        ProviderActivityId, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderSubagentStatus,
    },
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

/// A Session whose first Turn is running against the controlled Provider —
/// the state every Subagent test starts from, because only a working Turn's
/// Agent can spawn one.
struct WorkingTurn {
    server: RunningServer,
    provider_session: ControlledProviderSession,
    session_id: suru::protocol::SessionId,
    client: reqwest::Client,
}

async fn working_turn(state_dir: &std::path::Path, channel: &str) -> WorkingTurn {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir, channel).expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = reqwest::Client::new();
    let descriptor = server.descriptor().clone();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate the mapping".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let start = timeout(Duration::from_secs(1), provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-subagent", "high", "fast"),
    });
    timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    WorkingTurn {
        server,
        provider_session,
        session_id: created.session.id,
        client,
    }
}

/// The one Subagent Activity a snapshot holds, failing the test when the
/// Transcript carries none or more than one.
fn the_subagent_row(snapshot: &SessionSnapshot) -> &Activity {
    let mut rows = snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }));
    let row = rows.next().expect("the Transcript holds a Subagent row");
    assert!(
        rows.next().is_none(),
        "the Transcript holds exactly one Subagent row"
    );
    row
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

    // Drive commits into the child — content, then its settle — and then one
    // parent-visible change. The parent's is the first the stream announces,
    // which is what proves the child's commits rode no catalog stream.
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
    assert_eq!(
        crate::server_support::next_catalog_change(&mut catalog).await,
        SessionCatalogChange::WorkingChanged {
            session_id: fixture.session_id,
            working_since: None,
        },
        "the first announced change is the parent's own settle"
    );

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

/// Opens the Session catalog SSE stream and decodes its leading snapshot, so a
/// test can read what the catalog says it holds before any change arrives.
async fn open_catalog_stream_with_snapshot(
    descriptor: &suru::protocol::RuntimeDescriptor,
) -> (
    Vec<suru::protocol::SessionId>,
    impl futures_util::Stream<Item = suru::protocol::SessionCatalogUpdate> + Unpin + use<>,
) {
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/session-events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Session catalog stream")
        .error_for_status()
        .expect("the catalog stream authenticates");
    let mut events = Box::pin(response.bytes_stream().eventsource());
    let snapshot = loop {
        let event = timeout(Duration::from_secs(5), events.next())
            .await
            .expect("the catalog snapshot arrives")
            .expect("the catalog stream stays open")
            .expect("the catalog stream stays readable");
        if event.event == suru::protocol::SESSION_CATALOG_SNAPSHOT_EVENT {
            break serde_json::from_str::<suru::protocol::SessionCatalogSnapshot>(&event.data)
                .expect("decode the catalog snapshot");
        }
    };
    let updates = events.filter_map(|event| async move {
        let event = event.expect("the catalog stream stays open");
        (event.event == suru::protocol::SESSION_CATALOG_UPDATED_EVENT).then(|| {
            serde_json::from_str::<suru::protocol::SessionCatalogUpdate>(&event.data)
                .expect("decode a catalog update")
        })
    });
    (snapshot.session_ids, Box::pin(updates))
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
            ProviderEventAttribution::Subagent(subagent),
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
        .expect("delete parent Session");
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
