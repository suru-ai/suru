//! Subagent subtree roll-up: a Session's total carries everything its
//! delegated work consumed, to any depth, while each child keeps its own.

use crate::{
    provider_support::ControlledProvider,
    support::{
        WorkingTurn, open_catalog_stream_with_snapshot, read_session, read_session_until,
        working_turn,
    },
};
use suru::{
    protocol::{
        Activity, Cost, SessionCatalogChange, SessionCatalogUpdate, SessionId, SessionSnapshot,
        Usage, UsageTotal,
    },
    provider::{
        MeteredCost, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderSubagentStatus,
    },
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

fn active_total(fresh_input: u64, output: u64, usd: f64) -> UsageTotal {
    UsageTotal {
        fresh_input_tokens: Some(fresh_input),
        output_tokens: Some(output),
        cost: Cost::from_usd(usd),
        cost_is_partial: true,
        ..UsageTotal::default()
    }
}

/// The total a Session reports once every Turn under it has settled: nothing
/// is Working, so nothing more can accrue against the cost it reported.
fn settled_total(fresh_input: u64, output: u64, usd: f64) -> UsageTotal {
    UsageTotal {
        cost_is_partial: false,
        ..active_total(fresh_input, output, usd)
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
    let Activity::Subagent { session_id, .. } = owner
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { name: row_name, .. } if row_name == name))
        .expect("the named Subagent row opens")
    else {
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
    let change = crate::server_support::next_catalog_change_matching(catalog, |change| {
        matches!(change, SessionCatalogChange::UsageChanged { .. })
    })
    .await;
    let SessionCatalogChange::UsageChanged {
        session_id,
        total_usage,
    } = change
    else {
        unreachable!("the awaited change is a roll-up")
    };
    (session_id, total_usage)
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

    let parent = read_session_until_total(
        &fixture,
        fixture.session_id,
        active_total(6_000, 1_500, 0.25),
    )
    .await;
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
        Some(active_total(2_000, 500, 0.05)),
        "the roll-up stands apart from the parent's own Turns"
    );

    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(active_total(2_000, 500, 0.05)),
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
async fn a_whole_tree_cost_counts_once_and_retains_proven_later_cost_as_partial() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-cost-coverage-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let covered = ProviderSubagentId::new("covered-child");
    spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &covered,
        "Covered",
    )
    .await;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: Cost::from_usd(0.20)
                .map(|cost| MeteredCost::reported_subtree(cost, "provider-call-1")),
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(covered),
            ProviderEvent::Usage {
                usage: measured(2_000, 500),
                cost: Cost::from_usd(0.05).map(MeteredCost::reported),
            },
        )
        .await;

    let delayed = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the aggregate retains precedence over delayed child metering",
        |snapshot| {
            snapshot
                .total_usage()
                .is_some_and(|total| total.cost == Cost::from_usd(0.20) && total.cost_is_partial)
        },
    )
    .await;
    assert_eq!(delayed.total_usage().unwrap().cost, Cost::from_usd(0.20));

    let later = ProviderSubagentId::new("later-child");
    spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &later,
        "Later",
    )
    .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(later),
            ProviderEvent::Usage {
                usage: measured(900, 100),
                cost: Cost::from_usd(0.03).map(MeteredCost::reported),
            },
        )
        .await;
    let later = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "proven later work is added outside the aggregate",
        |snapshot| {
            snapshot
                .total_usage()
                .is_some_and(|total| total.cost == Cost::from_usd(0.23))
        },
    )
    .await;
    assert!(later.total_usage().unwrap().cost_is_partial);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("restart server");
    let restored = read_session(restarted.descriptor(), fixture.session_id).await;
    let restored_total = restored.total_usage().expect("restore the covered total");
    assert_eq!(restored_total.cost, Cost::from_usd(0.23));
    assert!(
        restored_total.cost_is_partial,
        "replay preserves the aggregate's temporal coverage"
    );
    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}

#[tokio::test]
async fn an_ancestor_report_suppresses_an_overlapping_nested_report() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "nested-cost-coverage-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let subagent = ProviderSubagentId::new("nested-report-child");
    let child_id = spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Measure",
    )
    .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(subagent),
            ProviderEvent::Usage {
                usage: measured(100, 10),
                cost: Cost::from_usd(0.05)
                    .map(|cost| MeteredCost::reported_subtree(cost, "child-lifetime")),
            },
        )
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(200, 20),
            cost: Cost::from_usd(0.10)
                .map(|cost| MeteredCost::reported_subtree(cost, "parent-lifetime")),
        })
        .await;

    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the ancestor aggregate wins without double counting",
        |snapshot| {
            snapshot
                .total_usage()
                .is_some_and(|total| total.cost == Cost::from_usd(0.10))
        },
    )
    .await;
    assert!(parent.total_usage().unwrap().cost_is_partial);
    assert_eq!(
        read_session(fixture.server.descriptor(), child_id)
            .await
            .total_usage()
            .and_then(|total| total.cost),
        Cost::from_usd(0.05),
        "the child's own Session retains its independently reported amount"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("restart server");
    let restored_parent = read_session(restarted.descriptor(), fixture.session_id).await;
    let restored_parent = restored_parent
        .total_usage()
        .expect("restore ancestor aggregate");
    assert_eq!(restored_parent.cost, Cost::from_usd(0.10));
    assert!(
        !restored_parent.cost_is_partial,
        "the restart settled every Turn, so the reported aggregate covers all the work there was"
    );
    assert_eq!(
        read_session(restarted.descriptor(), child_id)
            .await
            .total_usage()
            .and_then(|total| total.cost),
        Cost::from_usd(0.05),
        "restoration preserves the child's independently reported amount"
    );
    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}

#[tokio::test]
async fn output_without_a_price_keeps_a_known_total_partial_after_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "unpriced-output-coverage-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let subagent = ProviderSubagentId::new("unpriced-child");
    spawn_subagent(
        &fixture,
        ProviderEventAttribution::OwningSession,
        fixture.session_id,
        &subagent,
        "Write",
    )
    .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(200, 20),
            cost: Cost::from_usd(0.10)
                .map(|cost| MeteredCost::reported_subtree(cost, "output-lifetime")),
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Unmetered child output".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ] {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: subagent,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let partial = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "unpriced output marks the known subtotal partial",
        |snapshot| {
            snapshot
                .total_usage()
                .is_some_and(|total| total.cost == Cost::from_usd(0.10) && total.cost_is_partial)
        },
    )
    .await;
    assert!(partial.total_usage().unwrap().cost_is_partial);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("restart server");
    let restored = read_session(restarted.descriptor(), fixture.session_id).await;
    let restored = restored.total_usage().expect("restore known subtotal");
    assert_eq!(restored.cost, Cost::from_usd(0.10));
    assert!(restored.cost_is_partial);
    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}

#[tokio::test]
async fn a_settled_turn_with_output_and_its_own_price_stays_complete_after_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "priced-output-restoration-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Priced output".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::Usage {
            usage: measured(300, 30),
            cost: Cost::from_usd(0.04).map(MeteredCost::reported),
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let live = read_session(fixture.server.descriptor(), fixture.session_id)
        .await
        .total_usage()
        .expect("live priced Turn total");
    assert_eq!(live.cost, Cost::from_usd(0.04));
    assert!(!live.cost_is_partial);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("restart server");
    let restored = read_session(restarted.descriptor(), fixture.session_id)
        .await
        .total_usage()
        .expect("restored priced Turn total");
    assert_eq!(restored.cost, Cost::from_usd(0.04));
    assert!(!restored.cost_is_partial);
    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}

#[tokio::test]
async fn independent_reporting_lifetimes_on_one_turn_survive_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "independent-reporting-lifetimes-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(100, 10),
            cost: Cost::from_usd(0.10).map(|cost| MeteredCost::reported_subtree(cost, "process-1")),
        })
        .await;
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Work between reporting processes".to_owned(),
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
            usage: measured(100, 10),
            cost: Cost::from_usd(0.02).map(|cost| MeteredCost::reported_subtree(cost, "process-2")),
        })
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let live_snapshot = read_session(fixture.server.descriptor(), fixture.session_id).await;
    let live_details = live_snapshot.turns[0]
        .cost_details
        .as_ref()
        .expect("lifetime history");
    assert_eq!(live_details.prior.len(), 1);
    assert!(matches!(
        &live_details.prior[0].coverage,
        suru::protocol::CostCoverage::SessionSubtree { reporting_lifetime }
            if reporting_lifetime == "process-1"
    ));
    let live = live_snapshot.total_usage().expect("live lifetime total");
    assert_eq!(live.cost, Cost::from_usd(0.12));
    assert!(!live.cost_is_partial);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure restarted server"),
        runtime,
    )
    .await
    .expect("restart server");
    let restored_snapshot = read_session(restarted.descriptor(), fixture.session_id).await;
    assert_eq!(
        restored_snapshot.turns[0]
            .cost_details
            .as_ref()
            .expect("restored lifetime history")
            .prior
            .len(),
        1
    );
    let restored = restored_snapshot
        .total_usage()
        .expect("restored lifetime total");
    assert_eq!(restored.cost, Cost::from_usd(0.12));
    assert!(!restored.cost_is_partial);
    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
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

    read_session_until_total(&fixture, fixture.session_id, active_total(900, 100, 0.01)).await;
    let child = read_session(fixture.server.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(active_total(900, 100, 0.01)),
        "the Subagent in the middle rolls its own Subagent up too"
    );
    let grandchild = read_session(fixture.server.descriptor(), grandchild_id).await;
    assert_eq!(
        grandchild.total_usage(),
        Some(active_total(900, 100, 0.01)),
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
        (fixture.session_id, Some(active_total(2_000, 500, 0.05))),
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
    read_session_until_total(&fixture, fixture.session_id, active_total(2_000, 500, 0.05)).await;

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
        read_session_until_total(&fixture, fixture.session_id, active_total(3_000, 700, 0.07))
            .await;
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
    read_session_until_total(&fixture, session_id, active_total(6_000, 1_500, 0.25)).await;
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
        Some(settled_total(6_000, 1_500, 0.25)),
        "the roll-up comes back from the child's own stored Turns, settled by the restart"
    );
    let child = read_session(restarted.descriptor(), child_id).await;
    assert_eq!(
        child.total_usage(),
        Some(settled_total(2_000, 500, 0.05)),
        "a restored child still keeps its own"
    );

    restarted.shutdown().await.expect("stop restarted server");
}
