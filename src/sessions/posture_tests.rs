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
        sink.created(record, Vec::new());
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

/// A brokered Subagent's Session as the next process reads it back: a child
/// of `parent`, spawned through the Broker onto `provider`'s `model`, holding
/// `posture` as the process that stored it last derived it.
fn brokered(
    workspace: &Path,
    parent: SessionId,
    provider: &str,
    model: &str,
    posture: ApprovalPosture,
) -> PersistedSession {
    let mut record = PersistedSession {
        brokered: true,
        ..persisted(workspace, Some(parent))
    };
    record.snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
        options: Vec::new(),
    });
    record.snapshot.session.approval_posture = Some(SessionApprovalPosture {
        value: posture,
        pinned: false,
        application: ApprovalPostureApplication::Applying,
    });
    record.summary.session = record.snapshot.session.clone();
    record
}

fn codex_posture(
    approval_policy: CodexApprovalPolicy,
    sandbox_mode: CodexSandboxMode,
) -> ApprovalPosture {
    ApprovalPosture::Codex {
        approval_policy,
        sandbox_mode,
    }
}

/// A brokered Subagent owns its Provider actor across a restart: the next
/// process reads it back brokered. Hydration is when its spawner catches up
/// with the Settings, and so when its own reading is derived again from the
/// spawner's through ADR 0036's table — never handed the spawner's Claude
/// value itself, as a native Subagent riding the spawner's actor would be —
/// and nothing is owed to a Provider that does not exist yet.
#[tokio::test]
async fn a_brokered_subagent_read_back_by_the_next_process_keeps_its_own_actor_and_rederives_its_posture()
 {
    let directory = tempfile::tempdir().unwrap();
    let root = stale(directory.path(), None);
    let root_id = root.snapshot.session.id;
    let child = brokered(
        directory.path(),
        root_id,
        "codex",
        "gpt-5.5",
        codex_posture(CodexApprovalPolicy::Never, CodexSandboxMode::WorkspaceWrite),
    );
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
        snapshot(&store, root_id).session.approval_posture,
        Some(claude_posture(ClaudePermissionMode::Default)),
        "its spawner catches up with the Settings as ever"
    );
    let derived = SessionApprovalPosture {
        value: codex_posture(
            CodexApprovalPolicy::Untrusted,
            CodexSandboxMode::WorkspaceWrite,
        ),
        pinned: false,
        application: ApprovalPostureApplication::Applied,
    };
    assert_eq!(
        snapshot(&store, child_id).session.approval_posture,
        Some(derived),
        "and it reads Codex's value at its spawner's level, applied"
    );

    writer.shutdown().await.unwrap();
    let reloaded = repository.session(child_id).await.unwrap().unwrap();
    assert!(
        reloaded.brokered,
        "the next process reads it back brokered again"
    );
    assert_eq!(reloaded.snapshot.session.approval_posture, Some(derived));
}

/// A reading derived one level down is what the next level derives from, and
/// a native Subagent of a brokered one rides that brokered one's actor, so it
/// reads that posture rather than the top-level Session's.
#[tokio::test]
async fn each_level_of_a_tree_derives_from_the_level_above_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = stale(directory.path(), None);
    root.snapshot.session.approval_posture = Some(SessionApprovalPosture {
        value: ApprovalPosture::Claude {
            permission_mode: ClaudePermissionMode::DontAsk,
        },
        pinned: true,
        application: ApprovalPostureApplication::Applied,
    });
    root.summary.session = root.snapshot.session.clone();
    let root_id = root.snapshot.session.id;
    let stored = ApprovalPosture::Copilot {
        permissions: crate::protocol::CopilotPermissions::AllowAll,
    };
    let codex = brokered(directory.path(), root_id, "codex", "gpt-5.5", stored);
    let codex_id = codex.snapshot.session.id;
    let copilot = brokered(directory.path(), codex_id, "copilot", "gpt-4.1", stored);
    let copilot_id = copilot.snapshot.session.id;
    let claude = brokered(directory.path(), copilot_id, "claude", "haiku", stored);
    let claude_id = claude.snapshot.session.id;
    let native = persisted(directory.path(), Some(claude_id));
    let native_id = native.snapshot.session.id;
    let (_repository, writer, store) =
        deferred_store(directory.path(), vec![root, codex, copilot, claude, native]).await;

    store.hydrate(native_id).await.unwrap();

    let reading = |value, pinned| {
        Some(SessionApprovalPosture {
            value,
            pinned,
            application: ApprovalPostureApplication::Applied,
        })
    };
    assert_eq!(
        snapshot(&store, codex_id).session.approval_posture,
        reading(
            codex_posture(CodexApprovalPolicy::Never, CodexSandboxMode::WorkspaceWrite),
            true
        ),
        "Claude's dontAsk reads as Codex's never within the workspace, the root's pin carried"
    );
    let asks = ApprovalPosture::Copilot {
        permissions: crate::protocol::CopilotPermissions::Ask,
    };
    assert_eq!(
        snapshot(&store, copilot_id).session.approval_posture,
        reading(asks, true),
        "Copilot has no contained value that runs unasked, so it asks"
    );
    let default = ApprovalPosture::Claude {
        permission_mode: ClaudePermissionMode::Default,
    };
    assert_eq!(
        snapshot(&store, claude_id).session.approval_posture,
        reading(default, true),
        "and a Claude Subagent beneath it reads what Copilot's ask is, not the root's dontAsk"
    );
    assert_eq!(
        snapshot(&store, native_id).session.approval_posture,
        reading(default, true),
        "a native Subagent reads the posture of the brokered one whose actor it rides"
    );
    writer.shutdown().await.unwrap();
}

/// A Settings change that moves an unpinned top-level Session moves every
/// brokered Subagent beneath it, and each is owed its own value on its own
/// actor, as the top-level Session is on its.
#[tokio::test]
async fn a_settings_change_rederives_a_brokered_subagent_and_owes_its_own_actor_the_value() {
    let directory = tempfile::tempdir().unwrap();
    let root = stale(directory.path(), None);
    let root_id = root.snapshot.session.id;
    let child = brokered(
        directory.path(),
        root_id,
        "codex",
        "gpt-5.5",
        codex_posture(
            CodexApprovalPolicy::Untrusted,
            CodexSandboxMode::WorkspaceWrite,
        ),
    );
    let child_id = child.snapshot.session.id;
    let (_repository, writer, store) = deferred_store(directory.path(), vec![root, child]).await;
    store.hydrate(child_id).await.unwrap();

    let mut settings = EffectiveSettings::default();
    settings.provider.claude.permission_mode = ClaudePermissionMode::BypassPermissions;
    let updates = store.reconcile_approval_postures(&settings);

    let unrestricted = codex_posture(
        CodexApprovalPolicy::Never,
        CodexSandboxMode::DangerFullAccess,
    );
    let owed = updates
        .iter()
        .map(|update| (update.session_id, update.value))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        owed,
        [
            (
                root_id,
                ApprovalPosture::Claude {
                    permission_mode: ClaudePermissionMode::BypassPermissions
                }
            ),
            (child_id, unrestricted),
        ]
        .into(),
        "each actor is owed its own Provider's value"
    );
    assert_eq!(
        snapshot(&store, child_id).session.approval_posture,
        Some(SessionApprovalPosture {
            value: unrestricted,
            pinned: false,
            application: ApprovalPostureApplication::Applying,
        })
    );
    let update = updates
        .into_iter()
        .find(|update| update.session_id == child_id)
        .unwrap();
    assert!(store.approval_posture_update_is_pending(update));
    assert!(store.mark_approval_posture_application(update, ApprovalPostureApplication::Applied));
    assert_eq!(
        snapshot(&store, child_id)
            .session
            .approval_posture
            .map(|posture| posture.application),
        Some(ApprovalPostureApplication::Applied),
        "and reads it applied once its own Provider has taken it"
    );
    writer.shutdown().await.unwrap();
}

/// A brokered Subagent on its spawner's Provider acts under its spawner's
/// posture verbatim, a pin included; one on another Provider takes the value
/// ADR 0036's table gives for its spawner's, the pin still carried. Either is
/// recorded as applied, since no Provider runs for the child yet.
#[test]
fn a_brokered_subagents_posture_is_its_spawners_on_its_provider_and_the_tables_on_another() {
    let spawner = SessionApprovalPosture {
        value: ApprovalPosture::Claude {
            permission_mode: ClaudePermissionMode::Auto,
        },
        pinned: true,
        application: ApprovalPostureApplication::Applying,
    };
    let mut settings = EffectiveSettings::default();
    settings.provider.codex.approval_policy = CodexApprovalPolicy::Untrusted;
    let spawned = |spawner, provider: &str| {
        super::posture::brokered_subagent_posture(spawner, &ProviderId::new(provider), &settings)
    };

    assert_eq!(
        spawned(Some(spawner), "claude"),
        Some(SessionApprovalPosture {
            application: ApprovalPostureApplication::Applied,
            ..spawner
        }),
        "auto, which the table would read as acceptEdits, stands verbatim on Claude"
    );
    assert_eq!(
        spawned(Some(spawner), "codex"),
        Some(SessionApprovalPosture {
            value: codex_posture(
                CodexApprovalPolicy::OnRequest,
                CodexSandboxMode::WorkspaceWrite
            ),
            pinned: true,
            application: ApprovalPostureApplication::Applied,
        }),
        "on Codex it is Codex's value at auto's level rather than Codex's own Setting"
    );
    assert_eq!(
        spawned(None, "claude").map(|posture| posture.value),
        ApprovalPosture::for_provider(&ProviderId::new("claude"), &settings),
        "a spawner with no posture leaves the child its own Provider's Setting"
    );
    assert_eq!(
        spawned(Some(spawner), "gemini"),
        None,
        "and a Provider with no native posture Suru can name holds none"
    );
}
