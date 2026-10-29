//! Usage Metrics and Insights asked of the Server over its HTTP API. What
//! each read adds up is the Session store's to show; this shows the queries
//! and the reports cross the wire.

use crate::session_integration::{
    provider_support::ControlledProvider,
    support::{hosted_model, hosted_selection, read_session_until, working_turn},
};
use crate::support::PROGRESS_DEADLINE;
use std::time::Duration;
use suru::{
    pricing::CacheRates,
    protocol::{
        CacheInsights, Cost, CostTotal, CountedCost, CreateSessionRequest, DelegationInsights,
        EstimatedSaving, ExecutionDirectory, InitialPrompt, InsightsQuery, InsightsReport,
        InsightsTool, ModelId, PromptId, ProviderId, QUARTER_HOUR_MS, RuntimeDescriptor,
        SessionError, SessionErrorCode, SessionTimestamp, TURN_DURATION_BINS, ToolTally,
        TurnDurations, TurnInsights, Usage, UsageMetricsAggregate, UsageMetricsBreakdown,
        UsageMetricsInterval, UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery,
        UsageMetricsReport,
    },
    provider::{
        ErrandConsumption, ErrandCost, MeteredCost, ProviderActivityId, ProviderEvent,
        ProviderToolCallStatus,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

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
        // The Turn is working still, past what its Cost says.
        cost: CountedCost {
            amount: Cost::from_usd(0.25),
            is_partial: true,
        },
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
    assert_eq!(report.earliest_at, Some(started_at));
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
            .map(|session| (session.session_id, session.cost))
            .collect::<Vec<_>>(),
        vec![(
            fixture.session_id,
            CostTotal {
                cost: Cost::from_usd(0.25).unwrap(),
                is_partial: true,
            }
        )],
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

#[tokio::test]
async fn insights_are_read_over_the_api_and_a_backwards_interval_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "usage-insights-read").await;
    let descriptor = fixture.server.descriptor().clone();
    // The Provider prices the Turn's Model today, listing no cache-write
    // rate, as models.dev lists none for OpenAI's Models; its Turn read 3M
    // tokens from cache and wrote none.
    fixture.runtime.price_cache(
        ModelId::new("gpt-subagent"),
        CacheRates {
            input: 2.0,
            cache_read: Some(0.2),
            cache_write: None,
        },
    );
    let issue = ProviderActivityId::new("insights-issue");
    for event in [
        ProviderEvent::Usage {
            usage: Usage {
                fresh_input_tokens: Some(1_000_000),
                cache_read_tokens: Some(3_000_000),
                cache_write_tokens: Some(0),
                ..Usage::default()
            },
            cost: None,
        },
        ProviderEvent::ToolCallStarted {
            activity_id: issue.clone(),
            name: "create_issue".to_owned(),
            server: Some("github".to_owned()),
            input: None,
        },
        ProviderEvent::ToolCallCompleted {
            activity_id: issue,
            status: ProviderToolCallStatus::Failed,
            omitted_parts: 0,
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    let snapshot = read_session_until(
        &fixture.client,
        &descriptor,
        fixture.session_id,
        "the Turn Settles",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.settled_at.is_some())
        },
    )
    .await;
    let turn = &snapshot.turns[0];
    let started_at = turn.started_at.expect("a Turn begun now is timed");
    let took = turn.settled_at.expect("a Settled Turn is timed").0 - started_at.0;
    let ask = |interval: UsageMetricsInterval| {
        fixture
            .client
            .post(format!("{}/v1/usage-metrics/insights", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&InsightsQuery { interval })
            .send()
    };

    // The Tools are read from storage, which the Turn's Settle saves to
    // soon after, so the read is asked until the save has landed.
    let report = timeout(Duration::from_secs(10), async {
        loop {
            let response = ask(UsageMetricsInterval {
                start: started_at,
                end: SessionTimestamp(started_at.0 + 1),
            })
            .await
            .expect("send the Insights read");
            assert!(response.status().is_success(), "{}", response.status());
            let report = response.json::<InsightsReport>().await.unwrap();
            if !report.tools.is_empty() {
                return report;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Turn's Tool Call is saved");

    assert_eq!(
        report.tools,
        vec![ToolTally {
            tool: InsightsTool::Named {
                name: "create_issue".to_owned(),
                server: Some("github".to_owned()),
            },
            uses: 1,
            failed: 1,
        }]
    );
    let mut bins = [0; TURN_DURATION_BINS];
    bins[0] = 1;
    assert_eq!(
        report.turns,
        TurnInsights {
            durations: Some(TurnDurations {
                median_ms: took,
                p90_ms: took,
                longest_ms: took,
                bins,
            }),
            completed: 1,
            failed: 0,
            interrupted: 0,
        }
    );
    assert_eq!(
        report
            .quarter_hours
            .iter()
            .map(|quarter| (quarter.start, quarter.turns))
            .collect::<Vec<_>>(),
        vec![(
            SessionTimestamp(started_at.0 - started_at.0 % QUARTER_HOUR_MS),
            1
        )],
        "the Turn's UTC quarter-hour"
    );
    assert_eq!(
        report.delegation,
        DelegationInsights {
            own_sessions: CountedCost {
                amount: None,
                is_partial: true,
            },
            ..DelegationInsights::default()
        },
        "the user's own Session did the work, which nobody priced"
    );
    assert_eq!(
        report.cache,
        CacheInsights {
            fresh_input_tokens: Some(1_000_000),
            cache_read_tokens: Some(3_000_000),
            cache_write_tokens: Some(0),
            // 3M read at $0.20 rather than $2.00 a million.
            saving: EstimatedSaving {
                nano_usd: Some(5_400_000_000),
                is_partial: false,
            },
        },
        "estimated at the rates the hosting Provider states today"
    );

    let refused = ask(UsageMetricsInterval {
        start: SessionTimestamp(started_at.0 + 1),
        end: started_at,
    })
    .await
    .expect("send the backwards Insights read");
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    let refused = refused.json::<SessionError>().await.unwrap();
    assert_eq!(refused.code, SessionErrorCode::InvalidUsageMetricsQuery);
    fixture.server.shutdown().await.unwrap();
}

/// An Errand's Usage and Cost cross the wire under the reserved Errands key,
/// once the storage writer has landed its record.
#[tokio::test]
async fn an_errand_s_usage_and_cost_are_read_over_the_api_under_the_errands_key() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("controlled"),
        vec![hosted_model("controlled", "controlled-default")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "usage-metrics-errands").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: Some(hosted_selection("controlled", "controlled-default")),
            execution_directory: ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Count what Suru's own work costs".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds");
    let cost = Cost::from_usd(0.004).unwrap();
    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Title Errand reaches the Provider")
        .succeed_consuming(
            serde_json::json!({ "title": "Errands on the wire", "icon": "md-bug" }),
            ErrandConsumption {
                usage: Some(Usage {
                    fresh_input_tokens: Some(12),
                    output_tokens: Some(56),
                    ..Usage::default()
                }),
                cost: Some(ErrandCost::reported(cost)),
            },
        );

    let all_of_time = by_model(vec![UsageMetricsInterval {
        start: SessionTimestamp(0),
        end: SessionTimestamp(i64::MAX as u64),
    }]);
    let report = timeout(PROGRESS_DEADLINE, async {
        loop {
            let response = ask(&client, &descriptor, &all_of_time).await;
            assert!(response.status().is_success(), "{}", response.status());
            let report = response.json::<UsageMetricsReport>().await.unwrap();
            if report
                .keys
                .iter()
                .any(|label| label.key == UsageMetricsKey::Errands)
            {
                break report;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Errand's record lands");

    assert!(report.keys.contains(&UsageMetricsKeyLabel {
        key: UsageMetricsKey::Errands,
        label: "Errands".to_owned(),
    }));
    let errands = report.buckets[0]
        .by_key
        .iter()
        .find(|keyed| keyed.key == UsageMetricsKey::Errands)
        .expect("the bucket holds the Errand");
    assert_eq!(
        errands.aggregate,
        UsageMetricsAggregate {
            fresh_input_tokens: Some(12),
            output_tokens: Some(56),
            cost: CountedCost {
                amount: Some(cost),
                is_partial: false,
            },
            ..UsageMetricsAggregate::default()
        }
    );
    // The Session's first Turn never began, its Provider never answering
    // its start, so the whole is the Errand's alone: no Session, no Turn.
    assert_eq!(report.buckets[0].total, errands.aggregate);

    // Insights count it as Delegation's Errands, and its input in the
    // cache's figures, but in no quarter-hour, since it is no Turn.
    let insights = client
        .post(format!("{}/v1/usage-metrics/insights", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&InsightsQuery {
            interval: all_of_time.buckets[0],
        })
        .send()
        .await
        .expect("send the Insights read");
    assert!(insights.status().is_success(), "{}", insights.status());
    let insights = insights.json::<InsightsReport>().await.unwrap();
    assert_eq!(
        insights.delegation,
        DelegationInsights {
            errands: CountedCost {
                amount: Some(cost),
                is_partial: false,
            },
            ..DelegationInsights::default()
        }
    );
    assert_eq!(insights.delegation.total(), report.buckets[0].total.cost);
    assert_eq!(insights.cache.fresh_input_tokens, Some(12));
    assert_eq!(insights.quarter_hours, Vec::new());
    server.shutdown().await.unwrap();
}
