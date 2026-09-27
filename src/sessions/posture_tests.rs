//! Approval Posture reconciliation is measured at the store's hydration and
//! Settings seams, against a store whose histories are still deferred.

use std::path::Path;

use super::{SessionStore, restoration_tests::persisted};
use crate::{
    protocol::{
        AgentSelection, ApprovalPosture, ApprovalPostureApplication, ClaudePermissionMode,
        CodexApprovalPolicy, CodexSandboxMode, EffectiveSettings, ModelId, ProviderId,
        SessionApprovalPosture, SessionId, SessionListItem, SessionSnapshot,
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
        sink.created(record);
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

/// A brokered Subagent owns its Provider actor across a restart: the next
/// process reads it back brokered, so hydration leaves it the posture its
/// spawn gave it for its own Provider rather than handing it its spawner's,
/// as it would a native Subagent's riding the spawner's actor (ADR 0035).
#[tokio::test]
async fn a_brokered_subagent_read_back_by_the_next_process_keeps_its_own_actor_and_posture() {
    let directory = tempfile::tempdir().unwrap();
    let root = stale(directory.path(), None);
    let root_id = root.snapshot.session.id;
    let mut child = PersistedSession {
        brokered: true,
        ..persisted(directory.path(), Some(root_id))
    };
    let codex = SessionApprovalPosture {
        value: ApprovalPosture::Codex {
            approval_policy: CodexApprovalPolicy::Never,
            sandbox_mode: CodexSandboxMode::WorkspaceWrite,
        },
        pinned: false,
        application: ApprovalPostureApplication::Applied,
    };
    child.snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5.5"),
        options: Vec::new(),
    });
    child.snapshot.session.approval_posture = Some(codex);
    child.summary.session = child.snapshot.session.clone();
    let child_id = child.snapshot.session.id;
    let (repository, writer, store) = deferred_store(directory.path(), vec![root, child]).await;

    assert_eq!(
        store.actor_owner(child_id),
        Some(child_id),
        "a brokered Subagent owns its actor before its history is read"
    );
    store.hydrate(child_id).await.unwrap();
    assert_eq!(store.actor_owner(child_id), Some(child_id), "and after");
    assert_eq!(
        snapshot(&store, child_id).session.approval_posture,
        Some(codex),
        "its posture is its own, not its Claude spawner's"
    );
    assert_eq!(
        snapshot(&store, root_id).session.approval_posture,
        Some(claude_posture(ClaudePermissionMode::Default)),
        "while its spawner catches up with the Settings as ever"
    );

    writer.shutdown().await.unwrap();
    assert!(
        repository
            .session(child_id)
            .await
            .unwrap()
            .unwrap()
            .brokered,
        "and the next process reads it back brokered again"
    );
}

/// A brokered Subagent on its spawner's Provider acts under its spawner's
/// posture verbatim, a pin included; one on another Provider takes that
/// Provider's own Setting until ADR 0036's table replaces it. Either is
/// recorded as applied, since no Provider runs for the child yet.
#[test]
fn a_brokered_subagents_posture_is_its_spawners_on_its_provider_and_the_setting_on_another() {
    let spawner = SessionApprovalPosture {
        value: ApprovalPosture::Claude {
            permission_mode: ClaudePermissionMode::BypassPermissions,
        },
        pinned: true,
        application: ApprovalPostureApplication::Applying,
    };
    let mut settings = EffectiveSettings::default();
    settings.provider.codex.approval_policy = CodexApprovalPolicy::Untrusted;

    assert_eq!(
        super::posture::brokered_subagent_posture(
            Some(spawner),
            &ProviderId::new("claude"),
            &settings
        ),
        Some(SessionApprovalPosture {
            application: ApprovalPostureApplication::Applied,
            ..spawner
        })
    );
    assert_eq!(
        super::posture::brokered_subagent_posture(
            Some(spawner),
            &ProviderId::new("codex"),
            &settings
        ),
        Some(SessionApprovalPosture {
            value: ApprovalPosture::Codex {
                approval_policy: CodexApprovalPolicy::Untrusted,
                sandbox_mode: CodexSandboxMode::WorkspaceWrite,
            },
            pinned: false,
            application: ApprovalPostureApplication::Applied,
        })
    );
    assert_eq!(
        super::posture::brokered_subagent_posture(None, &ProviderId::new("claude"), &settings)
            .map(|posture| posture.value),
        ApprovalPosture::for_provider(&ProviderId::new("claude"), &settings),
        "a spawner with no posture leaves the child its own Provider's Setting"
    );
}
