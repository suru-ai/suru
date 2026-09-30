//! A brokered Subagent's Cost beneath its caller's. A brokered Subagent runs on
//! a Provider actor of its own (ADR 0035), so a whole-tree amount its caller's
//! Provider reports never holds its work: its Cost is added beneath that
//! amount, while a native Subagent's stays covered by the report of the
//! Session whose Provider ran it (ADR 0040). Each Session also carries its own
//! Cost beside its tree Cost.
//!
//! The caller runs on the double hosted as Claude, whose Cost is a cumulative
//! whole-tree amount per reporting lifetime.

use suru::protocol::CostTotal;
use suru::provider::ProviderEventAttribution;

use super::attribution::spawn_native;
use super::*;

fn usd(usd: f64) -> Cost {
    Cost::from_usd(usd).expect("a representable Cost")
}

fn measured(fresh_input: u64, output: u64) -> Usage {
    Usage {
        fresh_input_tokens: Some(fresh_input),
        output_tokens: Some(output),
        ..Usage::default()
    }
}

fn settled(cost: f64) -> Option<CostTotal> {
    Some(CostTotal {
        cost: usd(cost),
        is_partial: false,
    })
}

/// A Session's tree Cost and own Cost, as amounts.
fn amounts(snapshot: &SessionSnapshot) -> (Option<Cost>, Option<Cost>) {
    (
        snapshot.total_cost.map(|total| total.cost),
        snapshot.own_cost.map(|total| total.cost),
    )
}

#[tokio::test]
async fn a_brokered_subagent_is_counted_beneath_its_callers_whole_tree_cost() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-cost-subtree";
    let mut delegating = delegating(state_dir.path(), channel, None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: Some(MeteredCost::reported_subtree(usd(0.20), "caller-process")),
        })
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(2_000, 500),
            cost: Some(MeteredCost::reported(usd(0.05))),
        })
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;
    settle_callers_turn(&delegating, "the caller's Turn settles").await;

    let caller = read_until(
        &delegating.descriptor,
        delegating.caller,
        "the caller's tree Cost counts its brokered Subagent",
        |snapshot| snapshot.total_cost == settled(0.25),
    )
    .await;
    assert_eq!(
        caller.own_cost,
        settled(0.20),
        "the caller's own Cost is what its own Provider reported for its conversation"
    );
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(
        (child.total_cost, child.own_cost),
        (settled(0.05), settled(0.05)),
        "the Subagent's Session keeps its own Cost, with nothing beneath it"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
    drop((child_provider, delegating.caller_provider));

    let restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        (caller.total_cost, caller.own_cost),
        (settled(0.25), settled(0.20)),
        "a restored history reads as the live one did"
    );
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(
        (child.total_cost, child.own_cost),
        (settled(0.05), settled(0.05))
    );
    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagents_report_covers_its_own_native_subagent_and_counts_beneath_its_caller()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-cost-nested";
    let mut delegating = delegating(state_dir.path(), channel, None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("claude", "opus", json!({})))
        .await;
    let (child_provider, _) = run_child(
        &mut delegating.hosted.claude,
        default_selection(&claude_models()),
    )
    .await;
    let grandchild_id = spawn_native(
        &delegating.descriptor,
        child_id,
        &child_provider,
        "native-task",
    )
    .await;

    child_provider
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("native-task")),
            ProviderEvent::Usage {
                usage: measured(100, 10),
                cost: Some(MeteredCost::reported(usd(0.02))),
            },
        )
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(300, 30),
            cost: Some(MeteredCost::reported_subtree(usd(0.04), "child-process")),
        })
        .await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: Some(MeteredCost::reported_subtree(usd(0.10), "caller-process")),
        })
        .await;

    let expected = [
        (
            delegating.caller,
            (Some(usd(0.14)), Some(usd(0.10))),
            "the caller's report covers none of the brokered Subagent's tree",
        ),
        (
            child_id,
            (Some(usd(0.04)), Some(usd(0.04))),
            "the brokered Subagent's report covers its native Subagent's Cost",
        ),
        (
            grandchild_id,
            (Some(usd(0.02)), Some(usd(0.02))),
            "the native Subagent keeps its own Cost",
        ),
    ];
    for (session, reading, described) in expected {
        read_until(&delegating.descriptor, session, described, |snapshot| {
            amounts(snapshot) == reading
        })
        .await;
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
    drop((child_provider, delegating.caller_provider));

    let restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    for (session, reading, described) in expected {
        assert_eq!(
            amounts(&read_session(&descriptor, session).await),
            reading,
            "after a restart, {described}"
        );
    }
    restarted.server.shutdown().await.expect("shut down server");
}
