//! A Client reads the Usage Metrics of the Server its Outlook names, and one
//! turned toward a Remote is answered by the Remote exactly as its own Server
//! answers it. What the read adds up is the Session store's to show.
use super::*;
use suru::{
    managed_client::OutlookClient,
    protocol::{
        SessionTimestamp, UsageMetricsBreakdown, UsageMetricsBucket, UsageMetricsInterval,
        UsageMetricsQuery,
    },
};

fn both_ways(pair: &PairedServers) -> [(&'static str, OutlookClient); 2] {
    [
        ("locally", pair.serving_client.outlook(Outlook::Local)),
        (
            "through a Remote",
            pair.connecting_client
                .outlook(Outlook::Remote("workstation".to_owned())),
        ),
    ]
}

#[tokio::test]
async fn usage_metrics_are_read_from_the_server_the_outlook_names() {
    let pair = paired_servers("usage-metrics-outlook").await;
    let day = UsageMetricsInterval {
        start: SessionTimestamp(1_000),
        end: SessionTimestamp(86_401_000),
    };

    for (way, client) in both_ways(&pair) {
        let report = client
            .usage_metrics(&UsageMetricsQuery {
                buckets: vec![day],
                breakdown: UsageMetricsBreakdown::Model,
            })
            .await
            .unwrap_or_else(|error| panic!("{way}: {error:#}"));

        assert_eq!(
            report.buckets,
            vec![UsageMetricsBucket::default()],
            "{way}: one bucket, as asked, holding nothing"
        );
        assert_eq!(report.earliest_turn_at, None, "{way}");
    }
    pair.shutdown().await;
}
