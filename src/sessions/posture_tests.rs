//! Approval Posture reconciliation is measured at the store's hydration and
//! Settings seams, against a store whose histories are still deferred.

use std::path::Path;

use super::{SessionStore, restoration_tests::persisted};
use crate::{
    protocol::{
        AgentSelection, ApprovalPosture, ApprovalPostureApplication, ClaudePermissionMode,
        EffectiveSettings, ModelId, ProviderId, SessionApprovalPosture, SessionId, SessionListItem,
        SessionSnapshot,
    },
    storage::{PersistedSession, StorageRepository, StorageWriter},
};

fn claude_posture(permission_mode: ClaudePermissionMode) -> SessionApprovalPosture {
    SessionApprovalPosture {
        value: ApprovalPosture::Claude { permission_mode },
        pinned: false,
        application: ApprovalPostureApplication::Applied,
    }
}

/// A Claude Session whose stored unpinned posture predates the Settings the
/// next process opens with: it still carries `AcceptEdits` where the default
/// Setting says `Default`.
fn stale(workspace: &Path, parent: Option<SessionId>) -> PersistedSession {
    let mut record = persisted(workspace, parent);
    record.snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("claude-fable-5-1"),
        options: Vec::new(),
    });
    record.snapshot.session.approval_posture =
        Some(claude_posture(ClaudePermissionMode::AcceptEdits));
    record.summary.session = record.snapshot.session.clone();
    record
}

/// Persists `records` through one process's writer, then opens the store the
/// way the next process does: every history deferred, metadata restored.
async fn deferred_store(
    workspace: &Path,
    records: Vec<PersistedSession>,
) -> (StorageRepository, StorageWriter, SessionStore) {
    let repository = StorageRepository::open(workspace).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
    for record in records {
        sink.created(record.summary, record.snapshot, record.subagent_identity);
    }
    writer.shutdown().await.unwrap();
    let restored = repository.load_sessions().await.unwrap();
    assert_eq!(
        restored
            .deferred
            .as_ref()
            .map(|deferred| deferred.summaries.len()),
        Some(restored.readable.len()),
        "every history opens deferred"
    );
    let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());
    (repository, writer, store)
}

fn listed_posture(store: &SessionStore, id: SessionId) -> Option<SessionApprovalPosture> {
    store.list(None).into_iter().find_map(|item| match item {
        SessionListItem::Readable(summary) if summary.session.id == id => {
            summary.session.approval_posture
        }
        _ => None,
    })
}

fn snapshot(store: &SessionStore, id: SessionId) -> SessionSnapshot {
    store
        .subscribe(id)
        .expect("the Session is readable")
        .snapshot
}

/// A history that outlived its process also outlived the Settings that shaped
/// its posture. Loading it is when the posture catches up, and nothing is owed
/// to a live Provider because a deferred Session never has one.
#[tokio::test]
async fn hydration_brings_an_unpinned_posture_up_to_the_current_settings() {
    let directory = tempfile::tempdir().unwrap();
    let root = stale(directory.path(), None);
    let root_id = root.snapshot.session.id;
    let child = stale(directory.path(), Some(root_id));
    let child_id = child.snapshot.session.id;
    let (repository, writer, store) = deferred_store(directory.path(), vec![root, child]).await;

    store.hydrate(child_id).await.unwrap();

    let expected = claude_posture(ClaudePermissionMode::Default);
    assert_eq!(
        snapshot(&store, root_id).session.approval_posture,
        Some(expected)
    );
    assert_eq!(
        snapshot(&store, child_id).session.approval_posture,
        Some(expected),
        "a Subagent reads its root's posture"
    );
    assert_eq!(listed_posture(&store, root_id), Some(expected));
    assert!(
        store
            .current_approval_posture_update(root_id)
            .is_some_and(|update| { !store.approval_posture_update_is_pending(update) }),
        "nothing is owed to a Provider that does not exist yet"
    );

    writer.shutdown().await.unwrap();
    let reloaded = repository.session(root_id).await.unwrap().unwrap();
    assert_eq!(reloaded.snapshot.session.approval_posture, Some(expected));
}

/// A Settings change refreshes what is loaded and leaves what is not alone:
/// a deferred Session catches up when it is hydrated, not before.
#[tokio::test]
async fn a_settings_change_refreshes_loaded_sessions_and_passes_over_deferred_ones() {
    let directory = tempfile::tempdir().unwrap();
    let opened = stale(directory.path(), None);
    let opened_id = opened.snapshot.session.id;
    let deferred = stale(directory.path(), None);
    let deferred_id = deferred.snapshot.session.id;
    let (repository, writer, store) =
        deferred_store(directory.path(), vec![opened, deferred]).await;

    store.hydrate(opened_id).await.unwrap();
    let mut settings = EffectiveSettings::default();
    settings.provider.claude.permission_mode = ClaudePermissionMode::DontAsk;
    let updates = store.reconcile_approval_postures(&settings);

    let refreshed = SessionApprovalPosture {
        application: ApprovalPostureApplication::Applying,
        ..claude_posture(ClaudePermissionMode::DontAsk)
    };
    assert_eq!(
        updates
            .iter()
            .map(|update| update.session_id)
            .collect::<Vec<_>>(),
        vec![opened_id]
    );
    assert_eq!(
        snapshot(&store, opened_id).session.approval_posture,
        Some(refreshed)
    );
    assert_eq!(
        listed_posture(&store, deferred_id),
        Some(claude_posture(ClaudePermissionMode::AcceptEdits)),
        "a deferred history keeps its stored reading"
    );

    writer.shutdown().await.unwrap();
    let reloaded = repository.session(deferred_id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.snapshot.session.approval_posture,
        Some(claude_posture(ClaudePermissionMode::AcceptEdits))
    );
}

/// A change confined to one Session reconciles that Session's tree and no
/// other: the root follows the Settings, its Subagent reads the root, and an
/// unrelated root keeps whatever it last read until its own event.
#[tokio::test]
async fn reconciling_one_session_refreshes_its_tree_and_no_other() {
    let directory = tempfile::tempdir().unwrap();
    let root = stale(directory.path(), None);
    let root_id = root.snapshot.session.id;
    let child = stale(directory.path(), Some(root_id));
    let child_id = child.snapshot.session.id;
    let other = stale(directory.path(), None);
    let other_id = other.snapshot.session.id;
    let (_repository, writer, store) =
        deferred_store(directory.path(), vec![root, child, other]).await;
    store.hydrate(root_id).await.unwrap();
    store.hydrate(other_id).await.unwrap();
    let mut settings = EffectiveSettings::default();
    settings.provider.claude.permission_mode = ClaudePermissionMode::DontAsk;

    let update = store
        .reconcile_tree_approval_posture(child_id, &settings)
        .expect("the root owes its Provider the new value");

    let refreshed = SessionApprovalPosture {
        application: ApprovalPostureApplication::Applying,
        ..claude_posture(ClaudePermissionMode::DontAsk)
    };
    assert_eq!(update.session_id, root_id);
    assert_eq!(update.value, refreshed.value);
    assert_eq!(
        snapshot(&store, root_id).session.approval_posture,
        Some(refreshed)
    );
    assert_eq!(
        snapshot(&store, child_id).session.approval_posture,
        Some(refreshed),
        "a Subagent reads its root's posture"
    );
    assert_eq!(
        snapshot(&store, other_id).session.approval_posture,
        Some(claude_posture(ClaudePermissionMode::Default)),
        "an unrelated root is not this reconcile's to refresh"
    );
    writer.shutdown().await.unwrap();
}
