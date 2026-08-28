//! ADR 0015 at the provider-neutral seam: a Turn settles at the Provider's
//! own boundary while Subagents work on, Working derives from the Session's
//! subtree, and late output owed to those Subagents begins a Continuation
//! that the next delivered Prompt settles early.

use crate::support::{
    open_catalog_stream_with_snapshot, read_session_until, the_subagent_row, working_turn,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, MessageRole, PromptDelivery,
        PromptId, SessionCatalogChange, SessionId, SessionListItem, TurnStatus,
    },
    provider::{ProviderEvent, ProviderSubagentId, ProviderSubagentStatus},
};
use tokio::time::{Duration, timeout};

/// Spawns one Subagent into the fixture's working Turn and returns the child
/// Session the row names.
async fn spawn_subagent(
    fixture: &crate::support::WorkingTurn,
    subagent: &ProviderSubagentId,
) -> SessionId {
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Subagent row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };
    *child_id
}

#[tokio::test]
async fn a_turn_settles_at_the_providers_boundary_while_its_subagent_works_on() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "boundary-settle-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(&fixture, &subagent).await;

    // The Provider's own boundary passes while the Subagent still works.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Turn settles at the Provider's boundary",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(
        parent.turns[0].status,
        TurnStatus::Completed,
        "an open Subagent is no failure of the Turn's"
    );
    assert!(
        !parent
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Error { .. })),
        "no spurious failure lands in the Transcript: {:?}",
        parent.activities
    );
    let Activity::Subagent { status, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the Subagent's row works on past the settle"
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
        .expect("child Session remains readable")
        .json::<suru::protocol::SessionSnapshot>()
        .await
        .expect("decode child Session");
    assert_eq!(
        child.turns[0].status,
        TurnStatus::Active,
        "the child's Turn was not force-settled"
    );

    // The Provider's own settle still closes the row and the child, and the
    // settle alone provokes no Continuation.
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
        "the row settles on the Provider's signal",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                )
            })
        },
    )
    .await;
    let Activity::Subagent { duration_ms, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    assert!(duration_ms.is_some());
    assert_eq!(
        parent.turns.len(),
        1,
        "a Subagent's settle alone begins no Continuation"
    );
    let child = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "the child's Turn settles",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn working_reads_from_the_subtree_until_the_last_subagent_settles() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subtree-working-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    spawn_subagent(&fixture, &subagent).await;
    let (_, mut catalog) = open_catalog_stream_with_snapshot(fixture.server.descriptor()).await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    // The Turn settled, but the listing keeps saying Working: the reading now
    // anchors on the Subagent still running.
    let after_settle = crate::server_support::next_catalog_change(&mut catalog).await;
    let SessionCatalogChange::WorkingChanged {
        session_id,
        working_since: Some(_),
    } = after_settle
    else {
        panic!("the settle re-anchors Working on the Subagent, got {after_settle:?}");
    };
    assert_eq!(session_id, fixture.session_id);
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
    assert!(
        listed[0].working_since().is_some(),
        "the listing reads Working while the Subagent runs on"
    );

    // The last Subagent settling is what clears Working.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    assert_eq!(
        crate::server_support::next_catalog_change(&mut catalog).await,
        SessionCatalogChange::WorkingChanged {
            session_id: fixture.session_id,
            working_since: None,
        },
        "Working clears when the last Subagent settles"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn late_output_owed_to_a_subagent_begins_a_continuation_that_settles_like_any_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "continuation-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    spawn_subagent(&fixture, &subagent).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Late findings.".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
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
        "late output lands in a Continuation",
        |snapshot| snapshot.messages.len() == 2,
    )
    .await;
    assert_eq!(parent.turns.len(), 2, "the late output began one new Turn");
    let continuation = &parent.turns[1];
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation is the one Turn without a Prompt"
    );
    assert_eq!(continuation.status, TurnStatus::Active);
    assert!(
        continuation.started_at.is_some(),
        "a Continuation records when it began, like any Turn"
    );
    assert_eq!(parent.messages[1].role, MessageRole::Agent);
    assert_eq!(parent.messages[1].content, "Late findings.");
    assert_eq!(
        parent.messages[1].turn_id, continuation.id,
        "the late Message belongs to the Continuation"
    );

    // The Provider's next boundary settles the Continuation like any Turn.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Continuation settles at the Provider's boundary",
        |snapshot| snapshot.turns[1].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(parent.turns[1].status, TurnStatus::Completed);
    assert!(
        parent.turns[1].settled_at.is_some(),
        "a Continuation records when it settled, like any Turn"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_next_delivered_prompt_settles_a_stale_continuation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "stale-continuation-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    spawn_subagent(&fixture, &subagent).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::AgentMessageStarted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "a Continuation is running",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
    )
    .await;

    let prompt_id = PromptId::new();
    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            fixture.server.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Now try the harness".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("submit the next Prompt")
        .error_for_status()
        .expect("the Prompt is accepted while a Continuation runs");

    // The Prompt reaches the Provider as a fresh Turn — never as a steer of
    // the Continuation.
    timeout(Duration::from_secs(1), fixture.provider_session.next_turn())
        .await
        .expect("the Prompt begins a Turn of its own")
        .succeed();

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Prompt's Turn begins",
        |snapshot| snapshot.turns.len() == 3 && snapshot.turns[2].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(
        parent.turns[1].status,
        TurnStatus::Completed,
        "the delivered Prompt settled the stale Continuation"
    );
    assert!(parent.turns[1].settled_at.is_some());
    assert_eq!(parent.turns[2].prompt_id, Some(prompt_id));
    assert!(
        parent.messages.iter().any(|message| {
            message.turn_id == parent.turns[2].id && message.content == "Now try the harness"
        }),
        "the Prompt opens its own Turn rather than steering the Continuation"
    );
    let streaming = &parent.messages[1];
    assert_eq!(
        streaming.status,
        suru::protocol::MessageStatus::Completed,
        "the Continuation's streaming Message settles with it"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_begins_a_turn_while_an_earlier_turns_subagent_works_on() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "prompt-past-subagent-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    spawn_subagent(&fixture, &subagent).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let prompt_id = PromptId::new();
    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            fixture.server.descriptor().base_url,
            fixture.session_id,
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Keep going".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("submit the next Prompt")
        .error_for_status()
        .expect("the Prompt is accepted while the Subagent still works");
    timeout(Duration::from_secs(1), fixture.provider_session.next_turn())
        .await
        .expect("the Prompt begins a Turn while the Subagent runs")
        .succeed();

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the new Turn begins",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(parent.turns[1].prompt_id, Some(prompt_id));
    let Activity::Subagent { status, .. } = the_subagent_row(&parent) else {
        unreachable!()
    };
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "the earlier Turn's Subagent works on under the new Turn"
    );

    // The Subagent's settle still lands while the newer Turn runs.
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
        "the row settles under the newer Turn",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Subagent {
                        status: ActivityStatus::Completed,
                        ..
                    }
                )
            })
        },
    )
    .await;
    assert_eq!(
        parent.turns[1].status,
        TurnStatus::Active,
        "the newer Turn is untouched by the settle"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}
