//! Usage Metrics asked of the Server over its HTTP API. What the read adds up
//! is the Session store's to show; this shows the query and the report cross
//! the wire.

use crate::session_integration::support::{read_session_until, working_turn};
use suru::{
    protocol::{
        Cost, RuntimeDescriptor, SessionError, SessionErrorCode, SessionTimestamp, Usage,
        UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsInterval, UsageMetricsKey,
        UsageMetricsQuery, UsageMetricsReport,
    },
    provider::{MeteredCost, ProviderEvent},
};

fn by_model(buckets: Vec<UsageMetricsInterval>) -> UsageMetricsQuery {
    UsageMetricsQuery {
        buckets,
        breakdown: UsageMetricsBreakdown::Model,
        comparison: None,
        top_sessions: 0,
    }
}

async fn ask(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    query: &UsageMetricsQuery,
) -> reqwest::Response {
    client
        .post(format!("{}/v1/usage-metrics", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(query)
        .send()
        .await
        .expect("send the Usage Metrics read")
}

#[tokio::test]
async fn usage_metrics_are_read_over_the_api_and_a_bad_query_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "usage-metrics-read").await;
    let descriptor = fixture.server.descriptor().clone();
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: Usage {
                fresh_input_tokens: Some(4_000),
                output_tokens: Some(1_000),
                ..Usage::default()
            },
            cost: Some(MeteredCost::reported(Cost::from_usd(0.25).unwrap())),
        })
        .await;
    let snapshot = read_session_until(
        &fixture.client,
        &descriptor,
        fixture.session_id,
        "the Turn records its Usage",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.usage.is_some())
        },
    )
    .await;
    let started_at = snapshot.turns[0]
        .started_at
        .expect("a Turn begun now is timed");

    let the_turn = UsageMetricsInterval {
        start: started_at,
        end: SessionTimestamp(started_at.0 + 1),
    };
    let response = ask(
        &fixture.client,
        &descriptor,
        &UsageMetricsQuery {
            comparison: Some(the_turn),
            top_sessions: 3,
            ..by_model(vec![
                UsageMetricsInterval {
                    start: SessionTimestamp(0),
                    end: started_at,
                },
                the_turn,
            ])
        },
    )
    .await;

    assert!(response.status().is_success(), "{}", response.status());
    let report = response.json::<UsageMetricsReport>().await.unwrap();
    let measured = UsageMetricsAggregate {
        fresh_input_tokens: Some(4_000),
        output_tokens: Some(1_000),
        cost: Cost::from_usd(0.25),
        // The Turn is working still, past what its Cost says.
        is_cost_partial: true,
        sessions: 1,
        turns: 1,
        ..UsageMetricsAggregate::default()
    };
    assert_eq!(report.buckets.len(), 2);
    assert_eq!(report.buckets[0].total, UsageMetricsAggregate::default());
    assert_eq!(report.buckets[1].total, measured);
    assert_eq!(report.buckets[1].by_key.len(), 1);
    assert_eq!(report.buckets[1].by_key[0].aggregate, measured);
    assert_eq!(
        report
            .keys
            .iter()
            .map(|label| &label.key)
            .collect::<Vec<_>>(),
        vec![&report.buckets[1].by_key[0].key]
    );
    assert_eq!(report.earliest_turn_at, Some(started_at));
    assert_eq!(
        report.comparison,
        Some(measured),
        "the comparison asked for"
    );
    assert_eq!(report.across_buckets_and_untimed, measured);
    assert_eq!(
        report.across_buckets.total, measured,
        "the buckets taken together hold the one Session's Turn"
    );
    assert_eq!(
        report
            .top_sessions
            .iter()
            .map(|session| (session.session_id, session.cost, session.is_cost_partial))
            .collect::<Vec<_>>(),
        vec![(fixture.session_id, Cost::from_usd(0.25).unwrap(), true)],
        "the one Session that cost anything, its Turn working still"
    );

    // By Workspace, the Turn counts under its Session's, named as a
    // listing names it.
    let by_workspace = ask(
        &fixture.client,
        &descriptor,
        &UsageMetricsQuery {
            breakdown: UsageMetricsBreakdown::Workspace,
            ..by_model(vec![UsageMetricsInterval {
                start: started_at,
                end: SessionTimestamp(started_at.0 + 1),
            }])
        },
    )
    .await;
    assert!(by_workspace.status().is_success());
    let report = by_workspace.json::<UsageMetricsReport>().await.unwrap();
    let workspace = &snapshot.session.workspace;
    assert_eq!(report.keys.len(), 1);
    assert_eq!(
        report.keys[0].key,
        UsageMetricsKey::Workspace {
            workspace: workspace.id.clone()
        }
    );
    assert_eq!(
        Some(report.keys[0].label.as_str()),
        workspace.path.file_name().and_then(|name| name.to_str())
    );
    assert_eq!(report.buckets[0].by_key[0].aggregate, measured);

    let overlapping = UsageMetricsInterval {
        start: SessionTimestamp(0),
        end: SessionTimestamp(started_at.0 + 1),
    };
    let refused = ask(
        &fixture.client,
        &descriptor,
        &by_model(vec![overlapping, overlapping]),
    )
    .await;
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    let refused = refused.json::<SessionError>().await.unwrap();
    assert_eq!(refused.code, SessionErrorCode::InvalidUsageMetricsQuery);
    fixture.server.shutdown().await.unwrap();
}
