//! Usage Metrics read from a restored Session forest, as the next process
//! finds it: every Session's Turns in memory, and nothing yet read since.

use std::{collections::HashMap, path::Path};

use super::{Pairing, RemoteAct, SessionStore, UsageMetricsRefusal, restoration_tests::persisted};
use crate::protocol::{MAX_USAGE_METRICS_TOP_SESSIONS, SessionId, UsageMetricsSession};
use crate::{
    protocol::{
        AgentId, AgentIdentity, AgentSelection, Author, Cost, CostBasis, CostCoverage, CostDetails,
        KeyedUsageMetrics, MAX_USAGE_METRICS_BUCKETS, ModelAvailability, ModelCatalog,
        ModelDescriptor, ModelId, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
        SessionListItem, SessionTimestamp, Turn, TurnId, TurnStatus, Usage, UsageMetricsAggregate,
        UsageMetricsBreakdown, UsageMetricsBucket, UsageMetricsInterval, UsageMetricsKey,
        UsageMetricsKeyLabel, UsageMetricsQuery, UsageMetricsReport, UsageTotal, WorkspaceId,
        WorkspacePaths,
    },
    sidekick::SidekickWorkspace,
    storage::{
        PersistedSession, RestoredSessions, StorageRepository, StorageWriter, StoredWorkspace,
        WorkspacePresentation, WorkspaceProvenance,
    },
};

/// A Turn of `model` under `provider`, Settled as Completed, that began at
/// `started_at`, Settled at `settled_at`, and reported `output` tokens, with
/// no Cost.
pub(super) fn metered(
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
pub(super) const STUDIO: &str = "studio";
pub(super) const PAIRING: &str = "SHA256:studio";

pub(super) const FABLE: Option<(&str, &str)> = Some(("claude", "claude-fable-5-1"));

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
        comparison: None,
        top_sessions: 0,
    }
}

/// `tokens` of output from one Completed Turn of a top-level Session that
/// recorded no Cost, so its Cost is unknown.
fn output(tokens: u64) -> UsageMetricsAggregate {
    UsageMetricsAggregate {
        output_tokens: Some(tokens),
        is_cost_partial: true,
        sessions: 1,
        turns: 1,
        ..Default::default()
    }
}

/// `tokens` of output from `turns` Completed Turns of one top-level Session
/// that recorded no Cost.
fn output_of(tokens: u64, turns: u64) -> UsageMetricsAggregate {
    UsageMetricsAggregate {
        turns,
        ..output(tokens)
    }
}

/// `bucket`, its whole and every key's part the work of `sessions` top-level
/// Sessions and `subagents` Subagent Sessions, a Turn each.
fn worked_by(mut bucket: UsageMetricsBucket, sessions: u64, subagents: u64) -> UsageMetricsBucket {
    let counted = |aggregate: &mut UsageMetricsAggregate| {
        aggregate.sessions = sessions;
        aggregate.subagent_sessions = subagents;
        aggregate.turns = sessions + subagents;
    };
    counted(&mut bucket.total);
    bucket
        .by_key
        .iter_mut()
        .for_each(|keyed| counted(&mut keyed.aggregate));
    bucket
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

pub(super) async fn restore(
    directory: &Path,
    records: Vec<PersistedSession>,
) -> (SessionStore, StorageWriter) {
    restore_with_registry(directory, records, HashMap::new()).await
}

/// [`restore`], the Workspace Registry holding `registry`.
async fn restore_with_registry(
    directory: &Path,
    records: Vec<PersistedSession>,
    registry: HashMap<WorkspaceId, StoredWorkspace>,
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
        registry,
    );
    (store, writer)
}

/// A store restored from `records` as storage holds them, each history
/// deferred, so a Turn a stopped process left working still reads as Active,
/// as one working now does (ADR 0029).
pub(super) async fn restore_deferred(
    directory: &std::path::Path,
    records: Vec<crate::storage::PersistedSession>,
) -> (SessionStore, StorageWriter) {
    let repository = StorageRepository::open(directory).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    for record in records {
        sink.created(record, Vec::new());
    }
    writer.shutdown().await.unwrap();
    let restored = repository.load_sessions().await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());
    (store, writer)
}

fn read(store: &SessionStore, query: &UsageMetricsQuery) -> UsageMetricsReport {
    store
        .usage_metrics(
            query,
            &ModelCatalog {
                providers: Vec::new(),
            },
            &WorkspacePaths::default(),
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
            .usage_metrics(&by_model(&buckets), &catalog, &WorkspacePaths::default())
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
        // Working when its process stopped, so the store restores it as
        // Failed: it counts with what it had recorded by then.
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
        cost: None,
        is_cost_partial: true,
        estimated_cost: None,
        sessions: 1,
        subagent_sessions: 0,
        turns: 4,
        failed_turns: 2,
        interrupted_turns: 1,
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
        .usage_metrics(&by_model(&[(10, 20)]), &catalog, &WorkspacePaths::default())
        .unwrap();

    let model = |provider: &str| UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new("gpt-5.5"),
    };
    assert_eq!(
        report.keys,
        vec![
            // Each named for its Provider, since they read alike.
            UsageMetricsKeyLabel {
                key: model("codex"),
                label: "Codex · GPT-5.5".to_owned(),
            },
            // The catalog names no such Model, nor Provider, under Copilot:
            // the ids stand.
            UsageMetricsKeyLabel {
                key: model("copilot"),
                label: "copilot · gpt-5.5".to_owned(),
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
                aggregate: output_of(110, 2),
            },
            KeyedUsageMetrics {
                key: UsageMetricsKey::Unknown,
                aggregate: output(1000),
            },
        ]
    );
    assert_eq!(report.buckets[0].total, output_of(1111, 4));
    writer.shutdown().await.unwrap();
}

/// A Model `display_name` names under `provider`, as its catalog lists it.
fn listed(provider: &str, id: &str, display_name: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(id),
        display_name: display_name.to_owned(),
        description: String::new(),
        is_default: false,
        availability: ModelAvailability::Available,
        options: Vec::new(),
        details: Default::default(),
    }
}

#[tokio::test]
async fn models_that_would_read_alike_are_told_apart_in_the_keys_and_the_top_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        titled(
            directory.path(),
            "Through Copilot",
            vec![turn_scoped(Some(("copilot", "gpt-5.5")), 11, 0.40)],
        ),
        titled(
            directory.path(),
            "Through Codex",
            vec![turn_scoped(Some(("codex", "gpt-5.5")), 12, 0.30)],
        ),
        titled(
            directory.path(),
            "By alias",
            vec![
                turn_scoped(Some(("claude", "opus")), 13, 0.20),
                turn_scoped(Some(("claude", "claude-opus-5-5")), 14, 0.01),
            ],
        ),
        titled(
            directory.path(),
            "Pinned",
            vec![
                turn_scoped(Some(("claude", "claude-opus-5-5")), 15, 0.10),
                turn_scoped(SONNET, 16, 0.01),
            ],
        ),
    ];
    let (store, writer) = restore(directory.path(), records).await;
    let catalog = |provider: &str, models: Vec<ModelDescriptor>| ProviderModelCatalog {
        provider: ProviderId::new(provider),
        display_name: provider.to_owned(),
        models,
        status: ProviderCatalogStatus::Fresh,
    };
    let catalog = ModelCatalog {
        providers: vec![
            catalog(
                "claude",
                vec![
                    listed("claude", "opus", "Opus 5.5"),
                    listed("claude", "claude-sonnet-5-5", "Sonnet 5.5"),
                ],
            ),
            catalog("codex", vec![listed("codex", "gpt-5.5", "GPT-5.5")]),
            catalog("copilot", vec![listed("copilot", "gpt-5.5", "GPT-5.5")]),
        ],
    };

    let report = store
        .usage_metrics(&top(&[(10, 20)], 5), &catalog, &WorkspacePaths::default())
        .unwrap();

    let model = |provider: &str, model: &str| UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
    };
    assert_eq!(
        labels(&report),
        vec![
            // One Provider's alias and the id it stands for read alike once
            // the Provider's name is taken off the id: the alias says which
            // it is, and the id already does.
            (model("claude", "claude-opus-5-5"), "claude-opus-5-5"),
            (model("claude", "claude-sonnet-5-5"), "Sonnet 5.5"),
            (model("claude", "opus"), "Opus 5.5 (opus)"),
            // One name under two Providers: each says whose it is.
            (model("codex", "gpt-5.5"), "codex · GPT-5.5"),
            (model("copilot", "gpt-5.5"), "copilot · GPT-5.5"),
        ]
    );
    assert_eq!(
        report
            .top_sessions
            .iter()
            .map(|session| (session.title.as_str(), session.model.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("Through Copilot", Some("copilot · GPT-5.5")),
            ("Through Codex", Some("codex · GPT-5.5")),
            ("By alias", Some("Opus 5.5 (opus)")),
            ("Pinned", Some("claude-opus-5-5")),
        ],
        "a top Session's Model as the keys name it"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_name_under_two_providers_is_told_apart_whichever_provider_s_name_it_begins_with() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        metered(Some(("claude", "claude-opus-5-5")), Some(11), None, 1),
        metered(Some(("copilot", "claude-opus-5-5")), Some(12), None, 1),
        // Neither catalogued: their ids alike.
        metered(Some(("claude", "claude-sonnet-5-5")), Some(13), None, 1),
        metered(Some(("copilot", "claude-sonnet-5-5")), Some(14), None, 1),
        // An id one Provider's name begins, reached through another.
        metered(Some(("claude", "opus")), Some(15), None, 1),
        metered(Some(("copilot", "claude-opus-4")), Some(16), None, 1),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;
    let catalog =
        |provider: &str, display_name: &str, models: Vec<ModelDescriptor>| ProviderModelCatalog {
            provider: ProviderId::new(provider),
            display_name: display_name.to_owned(),
            models,
            status: ProviderCatalogStatus::Fresh,
        };
    let catalog = ModelCatalog {
        providers: vec![
            catalog(
                "claude",
                "Claude",
                vec![
                    listed("claude", "claude-opus-5-5", "Claude Opus 5.5"),
                    listed("claude", "opus", "Opus 4"),
                ],
            ),
            catalog(
                "copilot",
                "Copilot",
                vec![listed("copilot", "claude-opus-5-5", "Claude Opus 5.5")],
            ),
        ],
    };

    let report = store
        .usage_metrics(&by_model(&[(10, 20)]), &catalog, &WorkspacePaths::default())
        .unwrap();

    let model = |provider: &str, model: &str| UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
    };
    assert_eq!(
        labels(&report),
        vec![
            (
                model("claude", "claude-opus-5-5"),
                "Claude · Claude Opus 5.5"
            ),
            (
                model("claude", "claude-sonnet-5-5"),
                "Claude · claude-sonnet-5-5"
            ),
            // Claude's own name taken off Copilot's id reads as Claude's alias.
            (model("claude", "opus"), "Claude · Opus 4"),
            (model("copilot", "claude-opus-4"), "Copilot · claude-opus-4"),
            (
                model("copilot", "claude-opus-5-5"),
                "Copilot · Claude Opus 5.5"
            ),
            (
                model("copilot", "claude-sonnet-5-5"),
                "Copilot · claude-sonnet-5-5"
            ),
        ],
        "each named after its Provider as the Provider breakdown names it"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_top_session_s_model_is_told_apart_from_one_the_keys_do_not_name() {
    let directory = tempfile::tempdir().unwrap();
    let records = vec![
        titled(
            directory.path(),
            "Through Copilot",
            vec![turn_scoped(Some(("copilot", "gpt-5.5")), 11, 0.40)],
        ),
        titled(
            directory.path(),
            "Through Codex",
            vec![turn_scoped(Some(("codex", "gpt-5.5")), 12, 0.30)],
        ),
    ];
    let (store, writer) = restore(directory.path(), records).await;

    let report = read(
        &store,
        &UsageMetricsQuery {
            breakdown: UsageMetricsBreakdown::Workspace,
            ..top(&[(10, 20)], 5)
        },
    );

    assert_eq!(
        report
            .top_sessions
            .iter()
            .map(|session| session.model.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("copilot · gpt-5.5"), Some("codex · gpt-5.5")],
        "under a breakdown by Workspace, still told apart"
    );
    writer.shutdown().await.unwrap();
}

fn by(breakdown: UsageMetricsBreakdown, buckets: &[(u64, u64)]) -> UsageMetricsQuery {
    UsageMetricsQuery {
        breakdown,
        ..by_model(buckets)
    }
}

fn labels(report: &UsageMetricsReport) -> Vec<(UsageMetricsKey, &str)> {
    report
        .keys
        .iter()
        .map(|label| (label.key.clone(), label.label.as_str()))
        .collect()
}

/// Each key's part of `bucket`, in the order the report lists them.
fn parts(bucket: &UsageMetricsBucket) -> Vec<(UsageMetricsKey, UsageMetricsAggregate)> {
    bucket
        .by_key
        .iter()
        .map(|keyed| (keyed.key.clone(), keyed.aggregate))
        .collect()
}

fn provider(provider: &str) -> UsageMetricsKey {
    UsageMetricsKey::Provider {
        provider: ProviderId::new(provider),
    }
}

#[tokio::test]
async fn the_provider_breakdown_groups_every_model_under_its_provider_by_its_display_name() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        metered(Some(("codex", "gpt-5.5")), Some(11), None, 1),
        metered(Some(("copilot", "gpt-5.5")), Some(12), None, 10),
        metered(Some(("copilot", "claude-sonnet-5-5")), Some(13), None, 100),
        metered(FABLE, Some(14), None, 1000),
        metered(None, Some(15), None, 10_000),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;
    let catalog = |provider: &str, display_name: &str| ProviderModelCatalog {
        provider: ProviderId::new(provider),
        display_name: display_name.to_owned(),
        models: Vec::new(),
        status: ProviderCatalogStatus::Fresh,
    };
    let catalog = ModelCatalog {
        providers: vec![catalog("codex", "Codex"), catalog("copilot", "Copilot")],
    };

    let report = store
        .usage_metrics(
            &by(UsageMetricsBreakdown::Provider, &[(10, 20)]),
            &catalog,
            &WorkspacePaths::default(),
        )
        .unwrap();

    assert_eq!(
        labels(&report),
        vec![
            // The catalog names no such Provider: its id stands.
            (provider("claude"), "claude"),
            (provider("codex"), "Codex"),
            (provider("copilot"), "Copilot"),
            (UsageMetricsKey::Unknown, "Unknown"),
        ]
    );
    assert_eq!(
        parts(&report.buckets[0]),
        vec![
            (provider("claude"), output(1000)),
            (provider("codex"), output(1)),
            (provider("copilot"), output_of(110, 2)),
            (UsageMetricsKey::Unknown, output(10_000)),
        ]
    );
    assert_eq!(report.buckets[0].total, output_of(11_111, 5));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_workspace_breakdown_counts_each_turn_under_its_own_session_s_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let atlas = directory.path().join("atlas");
    let borealis = directory.path().join("borealis");
    let mut root = persisted(&atlas, None);
    root.snapshot.turns = vec![
        turn_scoped(OPUS, 11, 0.10),
        metered(CODEX, Some(12), None, 2),
    ];
    let root_id = root.snapshot.session.id;
    // A Subagent working in another Workspace counts there, by its own
    // Session, not its parent's.
    let mut subagent = persisted(&borealis, Some(root_id));
    subagent.brokered = true;
    subagent.snapshot.turns = vec![turn_scoped(SONNET, 13, 0.20)];
    let mut other = persisted(&borealis, None);
    other.snapshot.turns = vec![metered(None, Some(14), None, 100)];
    let workspace = |session: &PersistedSession| UsageMetricsKey::Workspace {
        workspace: session.snapshot.session.workspace.id.clone(),
    };
    let (atlas_key, borealis_key) = (workspace(&root), workspace(&other));
    let (store, writer) = restore(directory.path(), vec![root, subagent, other]).await;

    let report = read(&store, &by(UsageMetricsBreakdown::Workspace, &[(10, 20)]));

    let mut named = labels(&report);
    named.sort_by(|left, right| left.1.cmp(right.1));
    assert_eq!(
        named,
        vec![
            (atlas_key.clone(), "atlas"),
            (borealis_key.clone(), "borealis")
        ],
        "each Workspace by its name, and no Unknown: every Session has one"
    );
    let bucket = &report.buckets[0];
    let atlas = keyed(bucket, &atlas_key);
    assert_eq!(atlas.output_tokens, Some(3));
    assert_eq!(cost(&atlas), (usd(0.10), true, None));
    assert_eq!(sessions(&atlas), (1, 0));
    let borealis = keyed(bucket, &borealis_key);
    assert_eq!(borealis.output_tokens, Some(101));
    assert_eq!(cost(&borealis), (usd(0.20), true, None));
    assert_eq!(sessions(&borealis), (1, 1));
    assert_eq!(sessions(&bucket.total), (2, 1));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn workspaces_of_one_name_are_told_apart_by_where_they_are() {
    let directory = tempfile::tempdir().unwrap();
    let mine = directory.path().join("mine").join("suru");
    let theirs = directory.path().join("theirs").join("suru");
    let mut records = Vec::new();
    for path in [&mine, &theirs] {
        let mut session = persisted(path, None);
        session.snapshot.turns = vec![metered(FABLE, Some(11), None, 1)];
        records.push(session);
    }
    let (store, writer) = restore(directory.path(), records).await;

    let report = read(&store, &by(UsageMetricsBreakdown::Workspace, &[(10, 20)]));

    let paths = WorkspacePaths::default();
    let mut named = labels(&report)
        .into_iter()
        .map(|(_, label)| label.to_owned())
        .collect::<Vec<_>>();
    named.sort();
    assert_eq!(named, vec![paths.label(&mine), paths.label(&theirs)]);
    writer.shutdown().await.unwrap();
}

/// A Registry entry presenting its Workspace at `path`, or nowhere.
fn entry(path: Option<&Path>) -> StoredWorkspace {
    StoredWorkspace {
        path: path.map(Path::to_owned),
        presentation: Some(WorkspacePresentation::Directory),
        ..StoredWorkspace::known_by(WorkspaceProvenance::Session)
    }
}

#[tokio::test]
async fn a_workspace_is_named_as_its_registry_entry_presents_it_or_else_as_its_sessions_do() {
    let directory = tempfile::tempdir().unwrap();
    let mut presented = persisted(&directory.path().join("atlas"), None);
    presented.snapshot.turns = vec![metered(FABLE, Some(11), None, 1)];
    let mut pathless = persisted(&directory.path().join("borealis"), None);
    pathless.snapshot.turns = vec![metered(FABLE, Some(12), None, 1)];
    let id = |session: &PersistedSession| session.snapshot.session.workspace.id.clone();
    let (atlas, borealis) = (id(&presented), id(&pathless));
    // The Registry presents Atlas where it stands now, and Borealis
    // nowhere, as an entry from before rows kept a path does.
    let registry = HashMap::from([
        (
            atlas.clone(),
            entry(Some(&directory.path().join("atlas-moved"))),
        ),
        (borealis.clone(), entry(None)),
    ]);
    let (store, writer) =
        restore_with_registry(directory.path(), vec![presented, pathless], registry).await;

    let report = read(&store, &by(UsageMetricsBreakdown::Workspace, &[(10, 20)]));

    let mut named = labels(&report);
    named.sort_by(|left, right| left.1.cmp(right.1));
    let workspace = |workspace: WorkspaceId| UsageMetricsKey::Workspace { workspace };
    assert_eq!(
        named,
        vec![
            (workspace(atlas), "atlas-moved"),
            (workspace(borealis), "borealis"),
        ]
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_workspaces_presented_at_one_path_are_still_told_apart() {
    let directory = tempfile::tempdir().unwrap();
    let shared = directory.path().join("suru");
    let mut records = Vec::new();
    let mut registry = HashMap::new();
    for (index, turn_at) in [11, 12].into_iter().enumerate() {
        let mut session = persisted(&directory.path().join(format!("{index}")), None);
        session.snapshot.turns = vec![metered(FABLE, Some(turn_at), None, 1)];
        registry.insert(
            session.snapshot.session.workspace.id.clone(),
            entry(Some(&shared)),
        );
        records.push(session);
    }
    let (store, writer) = restore_with_registry(directory.path(), records, registry).await;

    let report = read(&store, &by(UsageMetricsBreakdown::Workspace, &[(10, 20)]));

    let label = |index: usize| report.keys[index].label.as_str();
    assert_ne!(label(0), label(1));
    let place = WorkspacePaths::default().label(&shared);
    assert!(
        label(0).starts_with(&place) && label(1).starts_with(&place),
        "{:?}",
        report.keys
    );
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

    assert_eq!(
        report.buckets,
        vec![worked_by(only(fable(), 1111), 2, 2)],
        "the Sidekick and its Subsession, and the two Subagents"
    );
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
    let (store, writer) = restore_deferred(directory.path(), vec![root, subagent]).await;
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

    assert_eq!(report.buckets, vec![worked_by(only(fable(), 11), 1, 1)]);
    assert_eq!(deferred(&store), [true, true], "no history was read in");
    assert_eq!(revisions(&store), before);
    writer.shutdown().await.unwrap();
}

const OPUS: Option<(&str, &str)> = Some(("claude", "claude-opus-5-5"));
const SONNET: Option<(&str, &str)> = Some(("claude", "claude-sonnet-5-5"));
const CODEX: Option<(&str, &str)> = Some(("codex", "gpt-5.5-codex"));

fn key(agent: Option<(&str, &str)>) -> UsageMetricsKey {
    let (provider, model) = agent.expect("a known Agent");
    UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
    }
}

fn usd(usd: f64) -> Option<Cost> {
    Some(Cost::from_usd(usd).unwrap())
}

/// `turn`, its Cost `usd` as `basis` says it was computed, covering what
/// `coverage` says and recorded at `recorded_at`.
fn priced(
    turn: Turn,
    usd: f64,
    basis: CostBasis,
    coverage: CostCoverage,
    recorded_at: u64,
) -> Turn {
    Turn {
        cost: Cost::from_usd(usd),
        cost_basis: Some(basis),
        cost_details: Some(CostDetails {
            coverage,
            recorded_at: SessionTimestamp(recorded_at),
            is_partial: false,
            prior: Vec::new(),
        }),
        ..turn
    }
}

/// A Turn of `agent` begun at `started_at` whose own Cost is `usd`, recorded
/// as it Settled one moment later.
fn turn_scoped(agent: Option<(&str, &str)>, started_at: u64, usd: f64) -> Turn {
    priced(
        metered(agent, Some(started_at), Some(started_at + 1), 1),
        usd,
        CostBasis::Reported,
        CostCoverage::Turn,
        started_at + 1,
    )
}

/// A Turn of `agent` begun at `started_at` carrying `usd`, the running total
/// of its Session's tree in `lifetime`, recorded as it Settled.
fn running_total(agent: Option<(&str, &str)>, started_at: u64, usd: f64, lifetime: &str) -> Turn {
    priced(
        metered(agent, Some(started_at), Some(started_at + 1), 1),
        usd,
        CostBasis::Reported,
        CostCoverage::SessionSubtree {
            reporting_lifetime: lifetime.to_owned(),
        },
        started_at + 1,
    )
}

/// What a bucket or a key's part of it says it cost: the amount, whether it
/// is partial, and its Estimated portion.
fn cost(aggregate: &UsageMetricsAggregate) -> (Option<Cost>, bool, Option<Cost>) {
    (
        aggregate.cost,
        aggregate.is_cost_partial,
        aggregate.estimated_cost,
    )
}

fn keyed(bucket: &UsageMetricsBucket, key: &UsageMetricsKey) -> UsageMetricsAggregate {
    bucket
        .by_key
        .iter()
        .find(|keyed| &keyed.key == key)
        .unwrap_or_else(|| panic!("{key:?} in {bucket:?}"))
        .aggregate
}

fn tree_cost(store: &SessionStore, id: crate::protocol::SessionId) -> Option<Cost> {
    store
        .subscribe(id)
        .unwrap()
        .snapshot
        .total_cost
        .map(|total| total.cost)
}

#[tokio::test]
async fn a_turn_scoped_cost_lands_in_its_turn_s_bucket_with_its_estimated_portion_apart() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        turn_scoped(OPUS, 11, 0.10),
        priced(
            metered(OPUS, Some(12), Some(13), 1),
            0.05,
            CostBasis::Estimated,
            CostCoverage::Turn,
            13,
        ),
        turn_scoped(CODEX, 25, 0.20),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30)]));

    assert_eq!(
        cost(&report.buckets[0].total),
        (usd(0.15), false, usd(0.05)),
        "the Estimated portion is part of the Cost, and stated apart"
    );
    assert_eq!(
        cost(&keyed(&report.buckets[0], &key(OPUS))),
        (usd(0.15), false, usd(0.05))
    );
    assert_eq!(
        cost(&report.buckets[1].total),
        (usd(0.20), false, None),
        "a Cost the Provider reported has no Estimated portion"
    );
    assert_eq!(
        cost(&keyed(&report.buckets[1], &key(CODEX))),
        (usd(0.20), false, None)
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_running_total_s_increments_land_in_the_buckets_of_the_turns_carrying_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        running_total(OPUS, 11, 0.10, "first-process"),
        running_total(OPUS, 21, 0.25, "first-process"),
        running_total(OPUS, 31, 0.30, "first-process"),
        // A second process restates its own total from nothing, and adds.
        running_total(OPUS, 41, 0.07, "second-process"),
    ];
    let id = session.snapshot.session.id;
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30), (30, 40), (40, 50)]));

    assert_eq!(
        report
            .buckets
            .iter()
            .map(|bucket| cost(&bucket.total))
            .collect::<Vec<_>>(),
        [0.10, 0.15, 0.05, 0.07].map(|amount| (usd(amount), false, None)),
        "each report adds what it adds to the highest total before it"
    );
    assert_eq!(
        tree_cost(&store, id),
        usd(0.37),
        "the buckets add up to the Cost each lifetime last reported"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_whole_tree_report_counts_under_the_model_carrying_it_and_a_brokered_subagent_under_its_own()
 {
    let directory = tempfile::tempdir().unwrap();
    let mut parent = persisted(directory.path(), None);
    // Claude's whole-tree report covers the native Subagent it ran, and says
    // nothing of how the amount divides between them.
    parent.snapshot.turns = vec![running_total(OPUS, 10, 0.30, "parent-process")];
    parent.snapshot.turns[0]
        .cost_details
        .as_mut()
        .unwrap()
        .recorded_at = SessionTimestamp(30);
    let parent_id = parent.snapshot.session.id;
    let mut native = persisted(directory.path(), Some(parent_id));
    native.snapshot.turns = vec![turn_scoped(SONNET, 12, 0.05)];
    let mut brokered = persisted(directory.path(), Some(parent_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![turn_scoped(CODEX, 14, 0.08)];
    let (store, writer) = restore(directory.path(), vec![parent, native, brokered]).await;

    let report = read(&store, &by_model(&[(0, 100)]));

    let bucket = &report.buckets[0];
    assert_eq!(cost(&keyed(bucket, &key(OPUS))), (usd(0.30), false, None));
    assert_eq!(
        cost(&keyed(bucket, &key(SONNET))),
        (None, false, None),
        "the native Subagent's own Cost is inside its parent's report"
    );
    assert_eq!(
        keyed(bucket, &key(SONNET)).output_tokens,
        Some(1),
        "though its Usage is its own"
    );
    assert_eq!(cost(&keyed(bucket, &key(CODEX))), (usd(0.08), false, None));
    assert_eq!(cost(&bucket.total), (usd(0.38), false, None));
    assert_eq!(tree_cost(&store, parent_id), usd(0.38));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_sidekick_s_subsessions_are_counted_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut sidekick = persisted(directory.path(), None);
    sidekick.snapshot.turns = vec![turn_scoped(OPUS, 11, 0.10)];
    let sidekick_id = sidekick.snapshot.session.id;
    let begun_by = Some(Author::Sidekick {
        session_id: sidekick_id,
        title: "Sidekick".to_owned(),
    });
    let mut subsession = persisted(directory.path(), None);
    subsession.snapshot.session.begun_by = begun_by.clone();
    subsession.summary.session.begun_by = begun_by;
    subsession.snapshot.turns = vec![turn_scoped(CODEX, 12, 0.05)];
    let subsession_id = subsession.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(subsession_id));
    subagent.snapshot.turns = vec![turn_scoped(CODEX, 13, 0.02)];
    let (store, writer) = restore(directory.path(), vec![sidekick, subsession, subagent]).await;

    let report = read(&store, &by_model(&[(10, 20)]));

    assert_eq!(
        cost(&report.buckets[0].total),
        (usd(0.17), false, None),
        "the Subsession is counted beneath its Sidekick, and not again as a root"
    );
    assert_eq!(
        cost(&keyed(&report.buckets[0], &key(CODEX))),
        (usd(0.07), false, None)
    );
    assert_eq!(tree_cost(&store, sidekick_id), usd(0.17));
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unpriced_turn_marks_its_bucket_its_key_and_the_untimed_figures_partial() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        turn_scoped(OPUS, 11, 0.10),
        metered(CODEX, Some(25), Some(26), 1),
        metered(OPUS, None, None, 1),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30)]));

    assert_eq!(cost(&report.buckets[0].total), (usd(0.10), false, None));
    assert_eq!(
        cost(&report.buckets[1].total),
        (None, true, None),
        "work with no known Cost is partial, not free"
    );
    assert_eq!(
        cost(&keyed(&report.buckets[1], &key(CODEX))),
        (None, true, None)
    );
    assert_eq!(cost(&report.untimed.total), (None, true, None));
    writer.shutdown().await.unwrap();
}

/// The Sessions a bucket or a key's part of it is the work of: top-level
/// Sessions, then Subagent Sessions.
fn sessions(aggregate: &UsageMetricsAggregate) -> (u64, u64) {
    (aggregate.sessions, aggregate.subagent_sessions)
}

#[tokio::test]
async fn a_session_that_worked_under_two_models_counts_once_under_each_and_once_in_the_total() {
    let directory = tempfile::tempdir().unwrap();
    let mut both = persisted(directory.path(), None);
    both.snapshot.turns = vec![
        metered(OPUS, Some(11), None, 1),
        metered(OPUS, Some(12), None, 1),
        metered(CODEX, Some(13), None, 1),
    ];
    let mut opus_only = persisted(directory.path(), None);
    // A Turn that reported no Usage is work all the same.
    opus_only.snapshot.turns = vec![Turn {
        usage: None,
        ..metered(OPUS, Some(14), None, 0)
    }];
    let (store, writer) = restore(directory.path(), vec![both, opus_only]).await;

    let report = read(&store, &by_model(&[(10, 20)]));

    let bucket = &report.buckets[0];
    assert_eq!(sessions(&keyed(bucket, &key(OPUS))), (2, 0));
    assert_eq!(sessions(&keyed(bucket, &key(CODEX))), (1, 0));
    assert_eq!(
        sessions(&bucket.total),
        (2, 0),
        "the keys add up to three, the Sessions are two"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_sessions_are_counted_apart_from_top_level_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![metered(OPUS, Some(11), None, 1)];
    let root_id = root.snapshot.session.id;
    let mut native = persisted(directory.path(), Some(root_id));
    native.snapshot.turns = vec![metered(SONNET, Some(12), None, 1)];
    let native_id = native.snapshot.session.id;
    // A Subagent's own Subagent is a Subagent too, however deep.
    let mut nested = persisted(directory.path(), Some(native_id));
    nested.snapshot.turns = vec![metered(SONNET, Some(13), None, 1)];
    let mut brokered = persisted(directory.path(), Some(root_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![metered(CODEX, Some(14), None, 1)];
    // A Subagent with no work in the bucket is not counted in it.
    let mut idle = persisted(directory.path(), Some(root_id));
    idle.snapshot.turns = vec![metered(SONNET, Some(25), None, 1)];
    let (store, writer) =
        restore(directory.path(), vec![root, native, nested, brokered, idle]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30)]));

    let bucket = &report.buckets[0];
    assert_eq!(sessions(&bucket.total), (1, 3));
    assert_eq!(sessions(&keyed(bucket, &key(OPUS))), (1, 0));
    assert_eq!(sessions(&keyed(bucket, &key(SONNET))), (0, 2));
    assert_eq!(sessions(&keyed(bucket, &key(CODEX))), (0, 1));
    assert_eq!(
        sessions(&report.buckets[1].total),
        (0, 1),
        "a Subagent's work counts it without its parent's"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_counts_in_every_bucket_it_worked_in_and_once_across_them_all() {
    let directory = tempfile::tempdir().unwrap();
    let mut daily = persisted(directory.path(), None);
    daily.snapshot.turns = vec![
        metered(OPUS, Some(11), None, 1),
        metered(OPUS, Some(21), None, 1),
        metered(CODEX, Some(22), None, 1),
        // Begun before the first bucket, placed in none of them.
        metered(OPUS, Some(5), None, 1),
        metered(OPUS, None, None, 1),
    ];
    let daily_id = daily.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(daily_id));
    subagent.snapshot.turns = vec![
        metered(CODEX, Some(12), None, 1),
        metered(CODEX, Some(31), None, 1),
    ];
    let mut once = persisted(directory.path(), None);
    once.snapshot.turns = vec![metered(OPUS, Some(32), None, 1)];
    let mut before = persisted(directory.path(), None);
    before.snapshot.turns = vec![metered(OPUS, Some(1), None, 1)];
    let (store, writer) = restore(directory.path(), vec![daily, subagent, once, before]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30), (30, 40)]));

    assert_eq!(
        report
            .buckets
            .iter()
            .map(|bucket| sessions(&bucket.total))
            .collect::<Vec<_>>(),
        vec![(1, 1), (1, 0), (1, 1)]
    );
    assert_eq!(
        sessions(&report.across_buckets.total),
        (2, 1),
        "across the buckets each Session counts once, though the buckets add to five"
    );
    assert_eq!(sessions(&keyed(&report.across_buckets, &key(OPUS))), (2, 0));
    assert_eq!(
        sessions(&keyed(&report.across_buckets, &key(CODEX))),
        (1, 1)
    );
    assert_eq!(
        report.across_buckets.total.output_tokens,
        Some(6),
        "the range holds what its buckets do, and no more"
    );
    assert_eq!(
        sessions(&report.untimed.total),
        (1, 0),
        "a Session with untimed work counts in the untimed figures"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_range_holds_the_cost_its_buckets_add_up_to() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        turn_scoped(OPUS, 11, 0.10),
        turn_scoped(OPUS, 21, 0.15),
        metered(CODEX, Some(22), Some(23), 1),
    ];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let report = read(&store, &by_model(&[(10, 20), (20, 30)]));

    assert_eq!(cost(&report.across_buckets.total), (usd(0.25), true, None));
    assert_eq!(
        cost(&keyed(&report.across_buckets, &key(OPUS))),
        (usd(0.25), false, None)
    );
    assert_eq!(
        cost(&keyed(&report.across_buckets, &key(CODEX))),
        (None, true, None)
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_comparison_interval_holds_what_its_turns_consumed_apart_from_the_buckets() {
    let directory = tempfile::tempdir().unwrap();
    let mut earlier = persisted(directory.path(), None);
    earlier.snapshot.turns = vec![
        turn_scoped(OPUS, 3, 0.10),
        Turn {
            status: TurnStatus::Interrupted,
            ..metered(CODEX, Some(6), None, 10)
        },
        // Before the comparison begins.
        metered(OPUS, Some(1), None, 1000),
        metered(OPUS, None, None, 10_000),
    ];
    let mut later = persisted(directory.path(), None);
    later.snapshot.turns = vec![
        // Placed by its Settle, in the comparison too.
        metered(OPUS, None, Some(9), 100),
        metered(OPUS, Some(15), None, 100_000),
    ];
    let (store, writer) = restore(directory.path(), vec![earlier, later]).await;

    let report = read(
        &store,
        &UsageMetricsQuery {
            comparison: Some(interval(2, 10)),
            ..by_model(&[(10, 20)])
        },
    );

    assert_eq!(
        report.comparison,
        Some(UsageMetricsAggregate {
            output_tokens: Some(111),
            cost: usd(0.10),
            is_cost_partial: true,
            sessions: 2,
            turns: 3,
            interrupted_turns: 1,
            ..Default::default()
        })
    );
    assert_eq!(
        report.across_buckets.total.output_tokens,
        Some(100_000),
        "the buckets hold none of the comparison's Turns"
    );
    assert_eq!(
        read(&store, &by_model(&[(10, 20)])).comparison,
        None,
        "a query asking for no comparison is answered with none"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_comparison_interval_may_overlap_the_buckets_but_not_end_before_it_begins() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![metered(OPUS, Some(15), None, 1)];
    let (store, writer) = restore(directory.path(), vec![session]).await;
    let with_comparison = |start, end| UsageMetricsQuery {
        comparison: Some(interval(start, end)),
        ..by_model(&[(10, 20)])
    };

    let overlapping = read(&store, &with_comparison(12, 18));
    assert_eq!(
        overlapping
            .comparison
            .map(|comparison| comparison.output_tokens),
        Some(Some(1))
    );
    assert_eq!(overlapping.buckets[0].total.output_tokens, Some(1));
    assert_eq!(
        store
            .usage_metrics(
                &with_comparison(10, 5),
                &ModelCatalog {
                    providers: Vec::new(),
                },
                &WorkspacePaths::default(),
            )
            .expect_err("the query is refused"),
        UsageMetricsRefusal::OutOfOrder
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_buckets_and_the_untimed_turns_together_count_each_session_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut both = persisted(directory.path(), None);
    both.snapshot.turns = vec![
        turn_scoped(OPUS, 11, 0.10),
        Turn {
            status: TurnStatus::Failed,
            ..metered(OPUS, None, None, 10)
        },
        // In no bucket, nor untimed: in neither figure.
        metered(OPUS, Some(1), None, 1000),
    ];
    let both_id = both.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(both_id));
    subagent.snapshot.turns = vec![metered(CODEX, None, None, 100)];
    let mut untimed = persisted(directory.path(), None);
    untimed.snapshot.turns = vec![metered(CODEX, None, None, 10_000)];
    let (store, writer) = restore(directory.path(), vec![both, subagent, untimed]).await;

    let report = read(&store, &by_model(&[(10, 20)]));

    assert_eq!(sessions(&report.across_buckets.total), (1, 0));
    assert_eq!(sessions(&report.untimed.total), (2, 1));
    assert_eq!(
        report.across_buckets_and_untimed,
        UsageMetricsAggregate {
            output_tokens: Some(10_111),
            cost: usd(0.10),
            is_cost_partial: true,
            sessions: 2,
            subagent_sessions: 1,
            turns: 4,
            failed_turns: 1,
            ..Default::default()
        },
        "a Session with timed and untimed work counts once, not once in each"
    );
    writer.shutdown().await.unwrap();
}

/// [`by_model`], asking for the `limit` Sessions that cost the most.
fn top(buckets: &[(u64, u64)], limit: usize) -> UsageMetricsQuery {
    UsageMetricsQuery {
        top_sessions: limit,
        ..by_model(buckets)
    }
}

/// Each top Session's identity and in-range Cost, most first.
fn ranked(report: &UsageMetricsReport) -> Vec<(SessionId, Option<Cost>, bool)> {
    report
        .top_sessions
        .iter()
        .map(|session| {
            (
                session.session_id,
                Some(session.cost),
                session.is_cost_partial,
            )
        })
        .collect()
}

/// A top-level Session in `workspace` titled `title` whose Turns are `turns`.
fn titled(workspace: &Path, title: &str, turns: Vec<Turn>) -> PersistedSession {
    let mut session = persisted(workspace, None);
    session.summary.title = title.to_owned();
    session.snapshot.turns = turns;
    session
}

#[tokio::test]
async fn the_top_sessions_are_ordered_by_their_cost_in_range_not_their_lifetime_cost() {
    let directory = tempfile::tempdir().unwrap();
    // The dearer over its lifetime, nearly all of it before the range.
    let lifelong = titled(
        directory.path(),
        "Lifelong",
        vec![turn_scoped(OPUS, 1, 0.90), turn_scoped(OPUS, 11, 0.10)],
    );
    let recent = titled(
        directory.path(),
        "Recent",
        vec![turn_scoped(CODEX, 12, 0.50)],
    );
    let (lifelong_id, recent_id) = (lifelong.snapshot.session.id, recent.snapshot.session.id);
    let (store, writer) = restore(directory.path(), vec![lifelong, recent]).await;

    let report = read(&store, &top(&[(10, 20)], 5));

    assert_eq!(
        ranked(&report),
        vec![
            (recent_id, usd(0.50), false),
            (lifelong_id, usd(0.10), false)
        ]
    );
    let name = directory
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        report.top_sessions[0],
        UsageMetricsSession {
            session_id: recent_id,
            title: "Recent".to_owned(),
            workspace: name,
            // The catalog names no Model: its id stands.
            model: Some("gpt-5.5-codex".to_owned()),
            cost: Cost::from_usd(0.50).unwrap(),
            is_cost_partial: false,
        }
    );
    assert_eq!(
        read(&store, &by_model(&[(10, 20)])).top_sessions,
        Vec::new(),
        "a query asking for none is answered with none"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn as_many_top_sessions_are_named_as_were_asked_for_and_too_many_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let sessions: Vec<PersistedSession> = [0.30, 0.10, 0.40, 0.20, 0.05]
        .into_iter()
        .enumerate()
        .map(|(at, usd)| {
            titled(
                directory.path(),
                &format!("{usd}"),
                vec![turn_scoped(OPUS, 11 + at as u64, usd)],
            )
        })
        .collect();
    let (store, writer) = restore(directory.path(), sessions).await;

    let titles = |limit| {
        read(&store, &top(&[(10, 20)], limit))
            .top_sessions
            .into_iter()
            .map(|session| session.title)
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(3), ["0.4", "0.3", "0.2"]);
    assert_eq!(titles(1), ["0.4"]);
    assert_eq!(titles(10), ["0.4", "0.3", "0.2", "0.1", "0.05"]);
    assert_eq!(
        store
            .usage_metrics(
                &top(&[(10, 20)], MAX_USAGE_METRICS_TOP_SESSIONS + 1),
                &ModelCatalog {
                    providers: Vec::new()
                },
                &WorkspacePaths::default(),
            )
            .unwrap_err(),
        UsageMetricsRefusal::TooManyTopSessions
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_partly_priced_session_is_flagged_and_one_of_no_known_cost_is_not_listed() {
    let directory = tempfile::tempdir().unwrap();
    let partly = titled(
        directory.path(),
        "Partly",
        vec![
            turn_scoped(OPUS, 11, 0.10),
            metered(OPUS, Some(12), None, 5),
        ],
    );
    let unpriced = titled(
        directory.path(),
        "Unpriced",
        vec![metered(OPUS, Some(13), None, 500)],
    );
    let partly_id = partly.snapshot.session.id;
    let (store, writer) = restore(directory.path(), vec![partly, unpriced]).await;

    let report = read(&store, &top(&[(10, 20)], 5));

    assert_eq!(ranked(&report), vec![(partly_id, usd(0.10), true)]);
    assert_eq!(
        read(&store, &top(&[(12, 20)], 5)).top_sessions,
        Vec::new(),
        "where none of the range's work has a known Cost, no Session is named"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_subagent_s_cost_counts_toward_its_top_level_session_and_a_subsession_stands_alone() {
    let directory = tempfile::tempdir().unwrap();
    let parent = titled(
        directory.path(),
        "Parent",
        vec![turn_scoped(OPUS, 11, 0.10)],
    );
    let parent_id = parent.snapshot.session.id;
    // A brokered Subagent keeps its own Cost, which its parent's work drove.
    let mut brokered = persisted(directory.path(), Some(parent_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![turn_scoped(CODEX, 12, 0.25)];
    let sidekick = titled(
        directory.path(),
        "Sidekick",
        vec![turn_scoped(OPUS, 13, 0.01)],
    );
    let sidekick_id = sidekick.snapshot.session.id;
    let begun_by = Some(Author::Sidekick {
        session_id: sidekick_id,
        title: "Sidekick".to_owned(),
    });
    let mut subsession = titled(
        directory.path(),
        "Subsession",
        vec![turn_scoped(SONNET, 14, 0.05)],
    );
    subsession.snapshot.session.begun_by = begun_by.clone();
    subsession.summary.session.begun_by = begun_by;
    let subsession_id = subsession.snapshot.session.id;
    let (store, writer) = restore(
        directory.path(),
        vec![parent, brokered, sidekick, subsession],
    )
    .await;

    let report = read(&store, &top(&[(10, 20)], 5));

    assert_eq!(
        ranked(&report),
        vec![
            (parent_id, usd(0.35), false),
            (subsession_id, usd(0.05), false),
            (sidekick_id, usd(0.01), false),
        ],
        "the Subagent's Cost rolls up into the Session it works for, and a \
         Subsession, a Session of its own, is listed apart from its Sidekick"
    );
    assert_eq!(
        report.top_sessions[0].model.as_deref(),
        Some("gpt-5.5-codex"),
        "the Model the Session's work cost the most under, its Subagent's too"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_top_session_s_model_is_the_one_its_work_cost_most_under_else_used_most_of() {
    let directory = tempfile::tempdir().unwrap();
    // Free work: no Model cost more than another, so the one used most.
    let free = titled(
        directory.path(),
        "Free",
        vec![
            turn_scoped(OPUS, 11, 0.0),
            Turn {
                usage: Some(Usage {
                    output_tokens: Some(1_000),
                    ..Default::default()
                }),
                ..turn_scoped(CODEX, 12, 0.0)
            },
        ],
    );
    let (store, writer) = restore(directory.path(), vec![free]).await;

    let report = read(&store, &top(&[(10, 20)], 5));

    assert_eq!(
        report.top_sessions.len(),
        1,
        "a known Cost of nothing is known"
    );
    assert_eq!(
        report.top_sessions[0].model.as_deref(),
        Some("gpt-5.5-codex")
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn top_sessions_that_cost_alike_stand_in_the_order_of_their_identities() {
    let directory = tempfile::tempdir().unwrap();
    let sessions: Vec<PersistedSession> = (0..4)
        .map(|at| {
            titled(
                directory.path(),
                &format!("{at}"),
                vec![turn_scoped(OPUS, 11 + at, 0.10)],
            )
        })
        .collect();
    let mut identities: Vec<SessionId> = sessions
        .iter()
        .map(|session| session.snapshot.session.id)
        .collect();
    identities.sort_by_key(|id| id.as_uuid());
    let (store, writer) = restore(directory.path(), sessions).await;

    for limit in [4, 2] {
        let named: Vec<SessionId> = read(&store, &top(&[(10, 20)], limit))
            .top_sessions
            .iter()
            .map(|session| session.session_id)
            .collect();
        assert_eq!(named, identities[..limit], "the first {limit} of a tie");
    }
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_top_session_s_workspace_is_told_apart_whatever_the_breakdown() {
    let directory = tempfile::tempdir().unwrap();
    let mine = directory.path().join("mine").join("suru");
    let theirs = directory.path().join("theirs").join("suru");
    let records = vec![
        titled(&mine, "Mine", vec![turn_scoped(OPUS, 11, 0.20)]),
        titled(&theirs, "Theirs", vec![turn_scoped(CODEX, 12, 0.10)]),
    ];
    let (store, writer) = restore(directory.path(), records).await;

    let paths = WorkspacePaths::default();
    let report = read(
        &store,
        &UsageMetricsQuery {
            breakdown: UsageMetricsBreakdown::Provider,
            ..top(&[(10, 20)], 5)
        },
    );

    assert_eq!(
        report
            .top_sessions
            .iter()
            .map(|session| (session.title.as_str(), session.workspace.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("Mine", paths.label(&mine)),
            ("Theirs", paths.label(&theirs))
        ],
        "two Workspaces of one name, by where each is, as a Workspace key is"
    );
    assert!(
        report
            .keys
            .iter()
            .all(|label| matches!(label.key, UsageMetricsKey::Provider { .. })),
        "naming the Sessions' Workspaces adds no Workspace key"
    );
    writer.shutdown().await.unwrap();
}
