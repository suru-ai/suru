//! Client-observable occupancy, ordering, ownership and durable restoration.
use crate::{
    provider_support::ControlledProvider,
    support::{
        controlled_selection, read_session, read_session_until, the_subagent_row, working_turn,
    },
};
use suru::{
    protocol::{Activity, ContextFill, SessionChange, TurnId, TurnStatus},
    provider::{
        ContextFillReport, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderSubagentStatus,
    },
    server::{self, ServerConfig},
};

fn reading(
    turn_id: Option<TurnId>,
    sequence: u64,
    tokens: u64,
    capacity: Option<u64>,
) -> ProviderEvent {
    ProviderEvent::ContextFill {
        report: ContextFillReport {
            turn_id,
            sequence,
            fill: ContextFill {
                occupied_tokens: tokens,
                capacity_tokens: capacity,
            },
        },
    }
}

#[tokio::test]
async fn context_fill_replaces_decreases_and_survives_settlement_and_restart() {
    let state = tempfile::tempdir().unwrap();
    let channel = "context-fill-restart";
    let fixture = working_turn(state.path(), channel).await;
    let before = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let turn = before.turns[0].id;
    assert_eq!(before.session.context_fill, None);
    for (sequence, tokens, capacity) in [
        (1, 12_400, Some(200_000)),
        (2, 4_000, None),
        (3, 0, Some(0)),
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(reading(Some(turn), sequence, tokens, capacity))
            .await;
        let snapshot = read_session_until(
            &fixture.client,
            fixture.server.descriptor(),
            fixture.session_id,
            "context report becomes client-visible",
            |s| {
                s.session
                    .context_fill
                    .is_some_and(|f| f.occupied_tokens == tokens)
            },
        )
        .await;
        assert_eq!(
            snapshot.session.context_fill.unwrap().capacity_tokens,
            capacity.filter(|c| *c > 0)
        );
        assert_eq!(snapshot.total_usage(), None);
    }
    // Repeated and out-of-order replies must not replace a genuinely reported zero.
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 2, 99_999, Some(200_000)))
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 3, 99_999, Some(200_000)))
        .await;
    assert_eq!(
        read_session(fixture.server.descriptor(), fixture.session_id)
            .await
            .session
            .context_fill
            .unwrap()
            .occupied_tokens,
        0
    );
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 4, 1500, Some(200_000)))
        .await;
    let settled = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "post-settlement request updates occupancy",
        |s| {
            s.session
                .context_fill
                .is_some_and(|f| f.occupied_tokens == 1500)
        },
    )
    .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.turns.len(),
        1,
        "report must not begin a Continuation"
    );
    let id = fixture.session_id;
    drop(fixture.provider_session);
    fixture.server.shutdown().await.unwrap();
    let (runtime, _provider) = ControlledProvider::new();
    let restarted =
        server::spawn_with_provider(ServerConfig::new(state.path(), channel).unwrap(), runtime)
            .await
            .unwrap();
    let restored = read_session(restarted.descriptor(), id).await;
    assert_eq!(restored.session.context_fill, settled.session.context_fill);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_context_is_independent_and_can_refresh_after_child_settlement() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "context-fill-child").await;
    let child = ProviderSubagentId::new("child");
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(None, 1, 12_400, Some(200_000)))
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: child.clone(),
            name: "Reader".into(),
            description: "Read".into(),
            delegation: None,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "child opens",
        |s| !s.activities.is_empty(),
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
            ProviderEventAttribution::Subagent(child.clone()),
            reading(None, 1, 50_000, Some(100_000)),
        )
        .await;
    let child_snapshot = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "child receives own report",
        |s| s.session.context_fill.is_some(),
    )
    .await;
    assert_eq!(
        child_snapshot.session.context_fill.unwrap().occupied_tokens,
        50_000
    );
    assert_eq!(
        read_session(fixture.server.descriptor(), fixture.session_id)
            .await
            .session
            .context_fill
            .unwrap()
            .occupied_tokens,
        12_400
    );
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: child.clone(),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(child),
            reading(None, 2, 1000, Some(100_000)),
        )
        .await;
    let settled = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        child_id,
        "child's delayed measurement arrives",
        |s| {
            s.session
                .context_fill
                .is_some_and(|f| f.occupied_tokens == 1000)
        },
    )
    .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn selection_preserves_working_measurement_but_a_new_model_invalidates_it() {
    let state = tempfile::tempdir().unwrap();
    let mut fixture = working_turn(state.path(), "context-fill-model").await;
    let first = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let turn = first.turns[0].id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 10, 12_400, Some(200_000)))
        .await;
    let selection = controlled_selection("different-model", "high", "fast");
    fixture
        .server
        .session_event_sink()
        .publish(
            fixture.session_id,
            vec![SessionChange::AgentSelectionChanged { selection }],
        )
        .await
        .unwrap();
    assert_eq!(
        read_session(fixture.server.descriptor(), fixture.session_id)
            .await
            .session
            .context_fill
            .unwrap()
            .occupied_tokens,
        12_400
    );
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            fixture.server.descriptor().base_url,
            fixture.session_id
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&suru::protocol::AdmitPromptRequest {
            prompt: suru::protocol::InitialPrompt {
                id: suru::protocol::PromptId::new(),
                text: "New model".into(),
                skill_invocations: Vec::new(),
            },
            delivery: suru::protocol::PromptDelivery::Queue,
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    fixture.provider_session.next_turn().await.succeed();
    let second = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "new Turn opens",
        |s| s.turns.len() == 2,
    )
    .await;
    assert_eq!(second.session.context_fill, None);
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 99, 99_999, Some(200_000)))
        .await;
    assert_eq!(
        read_session(fixture.server.descriptor(), fixture.session_id)
            .await
            .session
            .context_fill,
        None
    );
    let turn2 = second.turns[1].id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn2), 1, 4000, Some(100_000)))
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(reading(Some(turn), 100, 99_999, Some(200_000)))
        .await;
    assert_eq!(
        read_session(fixture.server.descriptor(), fixture.session_id)
            .await
            .session
            .context_fill
            .unwrap()
            .occupied_tokens,
        4000
    );
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            fixture.server.descriptor().base_url,
            fixture.session_id
        ))
        .bearer_auth(&fixture.server.descriptor().token)
        .json(&suru::protocol::AdmitPromptRequest {
            prompt: suru::protocol::InitialPrompt {
                id: suru::protocol::PromptId::new(),
                text: "Same model".into(),
                skill_invocations: Vec::new(),
            },
            delivery: suru::protocol::PromptDelivery::Queue,
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    fixture.provider_session.next_turn().await.succeed();
    let third = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "same-model Turn opens",
        |s| s.turns.len() == 3,
    )
    .await;
    assert_eq!(third.session.context_fill.unwrap().occupied_tokens, 4000);
    fixture.server.shutdown().await.unwrap();
}
