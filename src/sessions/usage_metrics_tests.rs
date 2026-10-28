//! Usage Metrics read from a restored Session forest, as the next process
//! finds it: every Session's Turns in memory, and nothing yet read since.

use std::path::Path;

use super::{Pairing, RemoteAct, SessionStore, UsageMetricsRefusal, restoration_tests::persisted};
use crate::{
    protocol::{
        AgentId, AgentIdentity, AgentSelection, Author, KeyedUsageMetrics,
        MAX_USAGE_METRICS_BUCKETS, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, SessionListItem, SessionTimestamp,
        Turn, TurnId, TurnStatus, Usage, UsageMetricsAggregate, UsageMetricsBreakdown,
        UsageMetricsBucket, UsageMetricsInterval, UsageMetricsKey, UsageMetricsKeyLabel,
        UsageMetricsQuery, UsageMetricsReport, UsageTotal,
    },
    sidekick::SidekickWorkspace,
    storage::{PersistedSession, RestoredSessions, StorageRepository, StorageWriter},
};

/// A Turn of `model` under `provider`, Settled as Completed, that began at
/// `started_at`, Settled at `settled_at`, and reported `output` tokens.
fn metered(
    agent: Option<(&str, &str)>,
    started_at: Option<u64>,
    settled_at: Option<u64>,
    output: u64,
) -> Turn {
    Turn {
        id: TurnId::new(),
        prompt_id: None,
        compaction_requested: false,
        agent: agent.map(|(provider, model)| AgentIdentity {
            agent: AgentId::new("main"),
            selection: AgentSelection {
                provider: ProviderId::new(provider),
                model: ModelId::new(model),
                options: Vec::new(),
            },
        }),
        status: TurnStatus::Completed,
        started_at: started_at.map(SessionTimestamp),
        settled_at: settled_at.map(SessionTimestamp),
        last_output_at: None,
        usage: Some(Usage {
            output_tokens: Some(output),
            ..Default::default()
        }),
        cost: None,
        cost_basis: None,
        cost_details: None,
    }
}

/// The Remote a Sidekick here began a Session on, and the key of the
/// Pairing it was begun through.
const STUDIO: &str = "studio";
const PAIRING: &str = "SHA256:studio";

const FABLE: Option<(&str, &str)> = Some(("claude", "claude-fable-5-1"));

fn fable() -> UsageMetricsKey {
    UsageMetricsKey::Model {
        provider: ProviderId::new("claude"),
        model: ModelId::new("claude-fable-5-1"),
    }
}

fn interval(start: u64, end: u64) -> UsageMetricsInterval {
    UsageMetricsInterval {
        start: SessionTimestamp(start),
        end: SessionTimestamp(end),
    }
}

fn by_model(buckets: &[(u64, u64)]) -> UsageMetricsQuery {
    UsageMetricsQuery {
        buckets: buckets
            .iter()
            .map(|(start, end)| interval(*start, *end))
            .collect(),
        breakdown: UsageMetricsBreakdown::Model,
    }
}

fn output(tokens: u64) -> UsageMetricsAggregate {
    UsageMetricsAggregate {
        output_tokens: Some(tokens),
        ..Default::default()
    }
}

/// A bucket holding `tokens` of output, every one of them under `key`.
fn only(key: UsageMetricsKey, tokens: u64) -> UsageMetricsBucket {
    UsageMetricsBucket {
        total: output(tokens),
        by_key: vec![KeyedUsageMetrics {
            key,
            aggregate: output(tokens),
        }],
    }
}

async fn restore(
    directory: &Path,
    records: Vec<PersistedSession>,
) -> (SessionStore, StorageWriter) {
    let repository = StorageRepository::open(directory).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    (store, writer)
}

fn read(store: &SessionStore, query: &UsageMetricsQuery) -> UsageMetricsReport {
    store
        .usage_metrics(
            query,
            &ModelCatalog {
                providers: Vec::new(),
            },
        )
        .expect("a well-formed query is answered")
}

#[tokio::test]
async fn a_turn_lands_in_the_bucket_of_its_start_falling_back_to_its_settle_then_to_untimed() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        // Begun in the first bucket and Settled in the second: its start
        // places it.
        metered(FABLE, Some(15), Some(25), 1),
        // Never said when it began: its Settle places it.
        metered(FABLE, None, Some(28), 10),
        // Neither: no bucket can place it.
        metered(FABLE, None, None, 100),
        // Begun before every bucket: it is in none, though it Settled in one.
        metered(FABLE, Some(5), Some(12), 1000),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30)]));

    assert_eq!(report.buckets, vec![only(fable(), 1), only(fable(), 10)]);
    assert_eq!(report.untimed, only(fable(), 100));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_query_with_too_many_buckets_or_buckets_out_of_order_or_overlapping_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let (store, writer) = restore(directory.path(), Vec::new()).await;
    let catalog = ModelCatalog {
        providers: Vec::new(),
    };
    let refusal = |buckets: Vec<(u64, u64)>| {
        store
            .usage_metrics(&by_model(&buckets), &catalog)
            .expect_err("the query is refused")
    };

    let too_many = (0..=MAX_USAGE_METRICS_BUCKETS as u64)
        .map(|hour| (hour * 10, hour * 10 + 10))
        .collect();
    assert_eq!(refusal(too_many), UsageMetricsRefusal::TooManyBuckets);
    assert_eq!(
        refusal(vec![(20, 30), (10, 20)]),
        UsageMetricsRefusal::OutOfOrder
    );
    assert_eq!(refusal(vec![(20, 10)]), UsageMetricsRefusal::OutOfOrder);
    assert_eq!(
        refusal(vec![(10, 25), (20, 30)]),
        UsageMetricsRefusal::Overlapping
    );
    // As many buckets as a query may ask for, each beginning where the one
    // before it ends, are answered.
    let most = (0..MAX_USAGE_METRICS_BUCKETS as u64)
        .map(|hour| (hour * 10, hour * 10 + 10))
        .collect::<Vec<_>>();
    assert_eq!(read(&store, &by_model(&most)).buckets.len(), most.len());
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_part_no_turn_reported_stays_absent_and_failed_interrupted_and_active_turns_count() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    let with = |status, usage: Usage| Turn {
        status,
        usage: Some(usage),
        ..metered(FABLE, Some(15), None, 0)
    };
    session.snapshot.turns = vec![
        with(
            TurnStatus::Failed,
            Usage {
                fresh_input_tokens: Some(100),
                output_tokens: Some(1),
                ..Default::default()
            },
        ),
        with(
            TurnStatus::Interrupted,
            Usage {
                cache_read_tokens: Some(20),
                output_tokens: Some(2),
                ..Default::default()
            },
        ),
        // Working still: it counts with what it has recorded so far.
        with(
            TurnStatus::Active,
            Usage {
                output_tokens: Some(4),
                reasoning_tokens: Some(3),
                ..Default::default()
            },
        ),
        // Reported nothing at all.
        Turn {
            usage: None,
            ..metered(FABLE, Some(16), Some(17), 0)
        },
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20)]));

    let expected = UsageMetricsAggregate {
        fresh_input_tokens: Some(100),
        cache_read_tokens: Some(20),
        cache_write_tokens: None,
        output_tokens: Some(7),
        reasoning_tokens: Some(3),
    };
    assert_eq!(
        report.buckets,
        vec![UsageMetricsBucket {
            total: expected,
            by_key: vec![KeyedUsageMetrics {
                key: fable(),
                aggregate: expected,
            }],
        }]
    );
    assert_eq!(
        report.buckets[0].total.blended_tokens(),
        Some(110),
        "fresh input, output and reasoning, as every surface blends them"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_model_under_two_providers_is_two_keys_and_a_turn_of_no_known_agent_is_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        metered(Some(("codex", "gpt-5.5")), Some(11), None, 1),
        metered(Some(("copilot", "gpt-5.5")), Some(12), None, 10),
        metered(Some(("copilot", "gpt-5.5")), Some(13), None, 100),
        metered(None, Some(14), None, 1000),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;
    let catalog = ModelCatalog {
        providers: vec![ProviderModelCatalog {
            provider: ProviderId::new("codex"),
            display_name: "Codex".to_owned(),
            models: vec![ModelDescriptor {
                provider: ProviderId::new("codex"),
                id: ModelId::new("gpt-5.5"),
                display_name: "GPT-5.5".to_owned(),
                description: String::new(),
                is_default: true,
                availability: ModelAvailability::Available,
                options: Vec::new(),
                details: Default::default(),
            }],
            status: ProviderCatalogStatus::Fresh,
        }],
    };

    let report = store
        .usage_metrics(&by_model(&[(10, 20)]), &catalog)
        .unwrap();

    let model = |provider: &str| UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new("gpt-5.5"),
    };
    assert_eq!(
        report.keys,
        vec![
            UsageMetricsKeyLabel {
                key: model("codex"),
                label: "GPT-5.5".to_owned(),
            },
            // The catalog names no such Model under Copilot: its id stands.
            UsageMetricsKeyLabel {
                key: model("copilot"),
                label: "gpt-5.5".to_owned(),
            },
            UsageMetricsKeyLabel {
                key: UsageMetricsKey::Unknown,
                label: "Unknown".to_owned(),
            },
        ]
    );
    assert_eq!(
        report.buckets[0].by_key,
        vec![
            KeyedUsageMetrics {
                key: model("codex"),
                aggregate: output(1),
            },
            KeyedUsageMetrics {
                key: model("copilot"),
                aggregate: output(110),
            },
            KeyedUsageMetrics {
                key: UsageMetricsKey::Unknown,
                aggregate: output(1000),
            },
        ]
    );
    assert_eq!(report.buckets[0].total, output(1111));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_and_subsession_turns_count_and_a_remotes_sessions_do_not() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let sidekick_workspace = SidekickWorkspace::beside(data.path()).unwrap();
    let mut sidekick = persisted(&sidekick_workspace.ensure().unwrap(), None);
    sidekick.snapshot.turns = vec![metered(FABLE, Some(11), None, 1)];
    let sidekick_id = sidekick.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(sidekick_id));
    subagent.snapshot.turns = vec![metered(FABLE, Some(12), None, 10)];
    let mut brokered = persisted(directory.path(), Some(sidekick_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![metered(FABLE, Some(13), None, 100)];
    let begun_by = Some(Author::Sidekick {
        session_id: sidekick_id,
        title: "Sidekick".to_owned(),
    });
    let mut subsession = persisted(directory.path(), None);
    subsession.snapshot.session.begun_by = begun_by.clone();
    subsession.summary.session.begun_by = begun_by;
    subsession.snapshot.turns = vec![metered(FABLE, Some(14), None, 1000)];
    let (store, writer) = restore(
        directory.path(),
        vec![sidekick, subagent, brokered, subsession],
    )
    .await;
    let store = store.with_sidekicks(sidekick_workspace, Vec::new());
    // The Sidekick began a Session on a Remote, which the Remote says has
    // consumed a great deal: it is the Remote's to count.
    let remote_session = persisted(directory.path(), None);
    let remote_id = remote_session.summary.session.id;
    store.record_remote_sidekick_act(
        sidekick_id,
        STUDIO,
        remote_id,
        RemoteAct {
            began: true,
            resolved: true,
            confirmed: true,
            pairing: PAIRING.to_owned(),
            beginning: None,
            evidence: None,
        },
    );
    let mut remote_summary = remote_session.summary;
    remote_summary.total_usage = Some(UsageTotal {
        output_tokens: Some(1_000_000),
        ..Default::default()
    });
    store.remote_read(
        STUDIO,
        &Pairing {
            fingerprint: PAIRING.to_owned(),
            generation: 1,
        },
        vec![SessionListItem::Readable(Box::new(remote_summary))],
        store.moment(),
    );
    assert_eq!(
        store.remotes_listed_with(sidekick_id),
        vec![STUDIO.to_owned()],
        "the Sidekick's tree lists the Remote's Session"
    );

    let report = read(&store, &by_model(&[(10, 20)]));

    assert_eq!(report.buckets, vec![only(fable(), 1111)]);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_earliest_timed_turn_is_reported_whichever_bucket_holds_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![
        metered(FABLE, Some(40), Some(45), 1),
        metered(FABLE, None, None, 1),
    ];
    let mut subagent = persisted(directory.path(), Some(root.snapshot.session.id));
    // Placed by its Settle, the earliest moment any Turn is placed at.
    subagent.snapshot.turns = vec![metered(FABLE, None, Some(30), 1)];
    let mut other = persisted(directory.path(), None);
    other.snapshot.turns = vec![metered(FABLE, Some(35), Some(50), 1)];
    let (store, writer) = restore(directory.path(), vec![root, subagent, other]).await;

    // Asked for no buckets at all, as a reader learning where all of time
    // begins does.
    let report = read(&store, &by_model(&[]));

    assert_eq!(report.earliest_turn_at, Some(SessionTimestamp(30)));
    assert!(report.buckets.is_empty());
    assert_eq!(report.untimed, only(fable(), 1));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn reading_never_hydrates_a_deferred_session_and_moves_no_revision() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![metered(FABLE, Some(11), Some(12), 1)];
    let root_id = root.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(root_id));
    subagent.snapshot.turns = vec![metered(FABLE, Some(13), Some(14), 10)];
    let subagent_id = subagent.snapshot.session.id;
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    sink.created(root, Vec::new());
    sink.created(subagent, Vec::new());
    writer.shutdown().await.unwrap();
    let restored = repository.load_sessions().await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());
    let deferred = |store: &SessionStore| {
        let state = store.state.lock().unwrap();
        [root_id, subagent_id].map(|id| state.is_deferred(id))
    };
    assert_eq!(
        deferred(&store),
        [true, true],
        "every history opens deferred"
    );
    let revisions = |store: &SessionStore| {
        let state = store.state.lock().unwrap();
        [root_id, subagent_id].map(|id| state.sessions[&id].snapshot.revision)
    };
    let before = revisions(&store);

    let report = read(&store, &by_model(&[(10, 20)]));

    assert_eq!(report.buckets, vec![only(fable(), 11)]);
    assert_eq!(deferred(&store), [true, true], "no history was read in");
    assert_eq!(revisions(&store), before);
    writer.shutdown().await.unwrap();
}
