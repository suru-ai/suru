//! Subagent subtree roll-up: a Session's total carries everything its
//! delegated work consumed, to any depth, while each child keeps its own.

use crate::{
    provider_support::ControlledProvider,
    support::{
        WorkingTurn, open_catalog_stream_with_snapshot, read_session, read_session_until,
        the_subagent_row, working_turn,
    },
};
use suru::{
    protocol::{
        Activity, Cost, SessionCatalogChange, SessionCatalogUpdate, SessionId, SessionSnapshot,
        Usage, UsageTotal,
    },
    provider::{MeteredCost, ProviderEvent, ProviderEventAttribution, ProviderSubagentId},
    server::{self, ServerConfig},
};

/// Usage a Provider reports in one call, sized so each caller's figures stay
/// legible in a total.
fn measured(fresh_input: u64, output: u64) -> Usage {
    Usage {
        fresh_input_tokens: Some(fresh_input),
        output_tokens: Some(output),
        ..Usage::default()
    }
}

fn totalling(fresh_input: u64, output: u64, usd: f64) -> UsageTotal {
    UsageTotal {
        fresh_input_tokens: Some(fresh_input),
        output_tokens: Some(output),
        cost: Cost::from_usd(usd),
        ..UsageTotal::default()
    }
}

/// Spawns one Subagent under `owner` — the owning Session's own Turn, or a
/// Subagent of its own — and answers with the child Session it opened.
async fn spawn_subagent(
    fixture: &WorkingTurn,
    attribution: ProviderEventAttribution,
    owner: SessionId,
    subagent: &ProviderSubagentId,
    name: &str,
) -> SessionId {
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            attribution,
            ProviderEvent::SubagentStarted {
                subagent_id: subagent.clone(),
                name: name.to_owned(),
                description: format!("{name} the provider seams"),
            },
        )
        .await;
    let owner = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        owner,
        "the Subagent row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Activity::Subagent { session_id, .. } = the_subagent_row(&owner) else {
        unreachable!()
    };
    *session_id
}

/// Reads `session` until its rolled-up total reaches `expected`, so a test
/// waits on the reading it means rather than on a revision count.
async fn read_session_until_total(
    fixture: &WorkingTurn,
    session: SessionId,
    expected: UsageTotal,
) -> SessionSnapshot {
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        session,
        "the rolled-up total reaches what the Provider reported",
        |snapshot| snapshot.total_usage() == Some(expected),
    )
    .await
}

/// The next roll-up the catalog announces, skipping the Working and Title
/// changes a working Session announces alongside it.
async fn next_usage_change(
    catalog: &mut (impl futures_util::Stream<Item = SessionCatalogUpdate> + Unpin),
) -> (SessionId, Option<UsageTotal>) {
    loop {
        if let SessionCatalogChange::UsageChanged {
            session_id,
            total_usage,
        } = crate::server_support::next_catalog_change(catalog).await
        {
            return (session_id, total_usage);
        }
    }
}

#[tokio::test]
async fn a_parents_total_carries_its_subagents_usage_while_the_child_keeps_its_own() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-usage-rollup-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Explore",
    )
    .await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: Cost::from_usd(0.20).map(MeteredCost::reported),
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::Usage {
                usage: measured(2_000, 500),
                cost: Cost::from_usd(0.05).map(MeteredCost::reported),
            },
        )
        .await;

    let parent =
        read_session_until_total(&fixture, fixture.session_id, totalling(6_000, 1_500, 0.25)).await;
    assert_eq!(
        parent.turns[0]
            .usage
            .as_ref()
            .and_then(Usage::blended_tokens),
        Some(5_000),
        "the parent's own Turn keeps only what the parent itself consumed"
    );
    assert_eq!(
        parent.subagent_usage,
        Some(totalling(2_000, 500, 0.05)),
        "the roll-up stands apart from the parent's own Turns"
    );

    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(totalling(2_000, 500, 0.05)),
        "the child's own total is its own Turn's Usage alone"
    );
    assert_eq!(
        child.subagent_usage, None,
        "a Subagent that delegated nothing rolls up nothing"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn roll_up_recurses_through_a_subagents_own_subagents() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-usage-depth-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let nested = ProviderSubagentId::new("task-2");
    let child_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Explore",
    )
    .await;
    let grandchild_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::Subagent(subagent),
        child_id,
        &nested,
        "Plan",
    )
    .await;

    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(nested),
            ProviderEvent::Usage {
                usage: measured(900, 100),
                cost: Cost::from_usd(0.01).map(MeteredCost::reported),
            },
        )
        .await;

    read_session_until_total(&fixture, fixture.session_id, totalling(900, 100, 0.01)).await;
    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(totalling(900, 100, 0.01)),
        "the Subagent in the middle rolls its own Subagent up too"
    );
    let grandchild = read_session(fixture.server.descriptor(), grandchild_id).await;
    assert_eq!(
        grandchild.total_usage(),
        Some(totalling(900, 100, 0.01)),
        "the deepest Session states what it consumed itself"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_catalog_announces_a_roll_up_without_the_session_open() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-usage-catalog-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Explore",
    )
    .await;
    let (listed, mut catalog) =
        open_catalog_stream_with_snapshot(fixture.server.descriptor()).await;
    assert_eq!(
        listed,
        vec![fixture.session_id],
        "the child rides no catalog of its own"
    );

    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::Usage {
                usage: measured(2_000, 500),
                cost: Cost::from_usd(0.05).map(MeteredCost::reported),
            },
        )
        .await;

    assert_eq!(
        next_usage_change(&mut catalog).await,
        (fixture.session_id, Some(totalling(2_000, 500, 0.05))),
        "the listed root announces what its subtree consumed"
    );
    assert_ne!(
        child_id, fixture.session_id,
        "the announcement names the listed root rather than the child"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn usage_owed_to_a_subagent_after_its_spawning_turn_settled_still_rolls_up() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-usage-continuation-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Explore",
    )
    .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    // The spawning Turn has settled and the Subagent works on: its Usage
    // still lands in the child, and still rolls up to the parent.
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::Usage {
                usage: measured(2_000, 500),
                cost: Cost::from_usd(0.05).map(MeteredCost::reported),
            },
        )
        .await;
    read_session_until_total(&fixture, fixture.session_id, totalling(2_000, 500, 0.05)).await;

    // Late output owed to that Subagent begins a Continuation, whose own
    // Usage counts like any Turn's.
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
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(1_000, 200),
            cost: Cost::from_usd(0.02).map(MeteredCost::reported),
        })
        .await;

    let parent =
        read_session_until_total(&fixture, fixture.session_id, totalling(3_000, 700, 0.07)).await;
    assert_eq!(
        parent.turns.len(),
        2,
        "the late output began the Continuation the Usage landed in"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restarted_server_derives_the_roll_up_again_from_the_turns_it_stored() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-usage-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let subagent = ProviderSubagentId::new("task-1");
    let child_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Explore",
    )
    .await;
    let session_id = fixture.session_id;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: Cost::from_usd(0.20).map(MeteredCost::reported),
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::Usage {
                usage: measured(2_000, 500),
                cost: Cost::from_usd(0.05).map(MeteredCost::reported),
            },
        )
        .await;
    read_session_until_total(&fixture, session_id, totalling(6_000, 1_500, 0.25)).await;
    drop(fixture.provider_session);
    fixture
        .server
        .shutdown()
        .await
        .expect("stop original server");

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("spawn restarted server");
    let parent = read_session(restarted.descriptor(), session_id).await;
    assert_eq!(
        parent.total_usage(),
        Some(totalling(6_000, 1_500, 0.25)),
        "the roll-up comes back from the child's own stored Turns"
    );
    let child = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(totalling(2_000, 500, 0.05)),
        "a restored child still keeps its own"
    );

    restarted.shutdown().await.expect("stop restarted server");
}
