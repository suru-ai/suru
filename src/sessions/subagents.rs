//! Opening a Subagent's child Session — the one Session creation a Prompt does
//! not drive, whichever route spawned the Subagent — beginning each later Turn
//! a resume of the Subagent begins in it, each opened by the Delegation that
//! began it, adding each Delegation that steers a Turn still working, and
//! finding the Session a resume continues by the identity its Provider stored
//! with it at the spawn.

use std::collections::{HashMap, HashSet};

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentSelection, Delegator, Message, MessageId,
    MessageRole, MessageStatus, ModelAvailability, PromptOrder, ProviderId, Session,
    SessionApprovalPosture, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStandingInputs, SessionStatus, SessionSummary, TranscriptItem, Turn, TurnId, TurnStatus,
};
use crate::provider::ProviderSubagentId;
use crate::storage::{PersistedSession, StorageSink, StoredSubagentIdentity};

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore, SessionStoreState,
    projection::active_turn_id,
    settlement::{OpenInterventions, TrailingCommandOutput, settle_in_flight_changes},
};

/// The child Session a spawn opened, named by what orchestration needs to
/// route the Subagent's stream: the Session itself, and the prompt-less Turn
/// its attributed events land in.
pub(crate) struct SpawnedSubagentSession {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
}

/// A Delegation delivered to a Subagent after its spawn — the one opening a
/// resume's Turn, or a steer into the Turn it works in: what the delegating
/// Agent asked, already normalized and capped the way an Agent Message's
/// content is, and the Session whose Agent sent it — the Subagent's parent,
/// or a sibling Subagent's.
pub(crate) struct DeliveredDelegation {
    pub(crate) delegating_session: SessionId,
    pub(crate) text: NormalizedText,
}

/// A Subagent the store holds by the identity its Provider gave it: its own
/// Session, the Session that spawned it — its place in the tree — and the
/// name its spawn's row carries, which every later row of it repeats.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSubagent {
    pub(crate) subagent_id: ProviderSubagentId,
    pub(crate) session_id: SessionId,
    pub(crate) spawner: SessionId,
    pub(crate) name: String,
}

impl SessionStore {
    /// Creates the child Session a Subagent runs in: parented to its spawner,
    /// titled from the spawn description — never an Errand — and opened with
    /// the one prompt-less Turn its attributed events land in. The spawn's
    /// Delegation, where the Provider reported its text, opens that Turn as a
    /// Message from the spawner's Agent, ahead of anything the Subagent does.
    /// The Provider's own identity for the Subagent is stored with it, so a
    /// resume naming it after a restart still finds this Session (ADR 0031).
    /// The child joins no listing and rides no catalog stream, so nothing is
    /// announced; it is reachable only through the row its spawner's
    /// Transcript shows.
    pub(crate) fn create_subagent(
        &self,
        parent_id: SessionId,
        identity: StoredSubagentIdentity,
        name: &str,
        description: &str,
        delegation: Option<NormalizedText>,
    ) -> anyhow::Result<SpawnedSubagentSession> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let parent = state
            .sessions
            .get(&parent_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let approval_posture = parent.snapshot.session.approval_posture;
        Ok(state.open_child_session(
            &self.storage,
            parent_id,
            ChildSession {
                title: subagent_title(name, description),
                selection: None,
                approval_posture,
                delegation,
                route: SubagentRoute::Native(identity),
            },
        ))
    }

    /// Begins the next Turn in a Subagent's existing Session, for a resume:
    /// the counterpart of [`Self::create_subagent`], which opened its first.
    /// The Session holds the agent's whole conversation, so the resumed work
    /// is a Turn of its own there rather than a Subagent of its own, and the
    /// Session keeps the Title its spawn gave it. Like the spawn's, the Turn
    /// begins without a Prompt, and without an Agent until the Provider
    /// reports the Model running this stretch. The resume's Delegation, where
    /// the Provider reported its text, opens the Turn in the same commit, so
    /// it stands ahead of anything the resumed Subagent does.
    ///
    /// A Turn still open in the Session — a Continuation the Subagent's own
    /// work began — Settles first, as worked: a Delegation delivered to begin
    /// a Turn settles such a Continuation rather than steering it, as a
    /// Prompt does (CONTEXT.md: Continuation). Both land in one commit, so no
    /// reader sees the Session between them.
    pub(crate) fn begin_subagent_turn(
        &self,
        session_id: SessionId,
        delegation: Option<DeliveredDelegation>,
    ) -> anyhow::Result<TurnId> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .begin_subagent_turn(&self.storage, session_id, delegation)
    }

    /// Adds a steer to the Turn a Subagent is working in (ADR 0032): the
    /// Delegation stands as a Message from the delegating Agent after
    /// everything the Subagent did before it received it, and begins no Turn.
    /// The Delegation that began the Turn, where the Provider reported it only
    /// after the spawn or resume that began it, is added the same way, and so
    /// stands at the Turn's head, nothing having landed before it. A Turn
    /// that has Settled accepts no further Delegation, since a steer is only
    /// ever delivered into work still going.
    pub(crate) fn deliver_delegation(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        delegation: DeliveredDelegation,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        if !record.snapshot.session.is_subagent() {
            return Err(anyhow!(
                "Only a Subagent's Session is steered by a Delegation"
            ));
        }
        if active_turn_id(&record.snapshot)? != Some(turn_id) {
            return Err(anyhow!(
                "A steer is delivered only into the Turn still working"
            ));
        }
        let message = delegation_message(
            turn_id,
            delegator(&state.sessions, delegation.delegating_session),
            delegation.text,
        );
        state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::MessageAdded { message }],
        )?;
        Ok(())
    }

    /// Every Subagent riding the Provider actor `owner` owns whose Session the
    /// store holds by an identity `provider` minted, nearest the top first.
    /// This is what a Provider connection that resumes its conversation —
    /// after a restart above all — relearns its Subagents from: the identities
    /// are the connection's own, and the connection belongs to the Session
    /// that owns its actor, so no identity here can resolve into another tree,
    /// nor below a Subagent with an actor of its own, whose connection minted
    /// the identities beneath it.
    pub(crate) fn stored_subagents(
        &self,
        owner: SessionId,
        provider: &ProviderId,
    ) -> Vec<StoredSubagent> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state
            .identified_subagents(owner, provider)
            .into_iter()
            .map(|subagent| StoredSubagent {
                subagent_id: subagent.subagent_id.clone(),
                session_id: subagent.session_id,
                spawner: subagent.spawner,
                // The name the spawn's row carries in the spawner's Transcript.
                name: delegator(&state.sessions, subagent.session_id)
                    .name
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// The Session of the native Subagent that `owner`'s Provider connection
    /// knows by `subagent_id`, `owner` being the Session a Broker token names:
    /// a Subagent riding the Provider actor `owner` owns, at any depth, held
    /// by that identity as `owner`'s own Provider minted it. An identity is
    /// its connection's alone, so none resolves into another tree, nor
    /// beneath a Subagent with an actor of its own; one the connection named
    /// twice finds the Subagent a resume after a restart would (see
    /// [`Self::stored_subagents`]). `None` for an identity nothing riding the
    /// actor carries — `owner` itself is known by none — and for an `owner`
    /// that selected no Provider, as a native Subagent's Session never does.
    pub(crate) fn native_subagent_riding(
        &self,
        owner: SessionId,
        subagent_id: &ProviderSubagentId,
    ) -> Option<SessionId> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let provider = &state
            .sessions
            .get(&owner)?
            .snapshot
            .session
            .agent_selection
            .as_ref()?
            .provider;
        state
            .identified_subagents(owner, provider)
            .into_iter()
            .find(|subagent| subagent.subagent_id == subagent_id)
            .map(|subagent| subagent.session_id)
    }
}

/// A native Subagent the store holds by the identity its Provider gave it,
/// read where the store holds it: its own Session, the Session that spawned
/// it, and that identity.
struct IdentifiedSubagent<'a> {
    session_id: SessionId,
    spawner: SessionId,
    subagent_id: &'a ProviderSubagentId,
}

impl SessionStoreState {
    /// Every Subagent riding the Provider actor `owner` owns whose Session the
    /// store holds by an identity `provider` minted: nearest the top first,
    /// and each Session's children in spawn order, so an identity a Provider
    /// ever named twice is met first where it was first carried. The walk
    /// passes into no Subagent with an actor of its own, whose Provider
    /// connection — not `owner`'s — minted the identities beneath it.
    fn identified_subagents(
        &self,
        owner: SessionId,
        provider: &ProviderId,
    ) -> Vec<IdentifiedSubagent<'_>> {
        let mut tree = vec![owner];
        let mut identified = Vec::new();
        let mut visit = 0;
        while visit < tree.len() {
            let spawner = tree[visit];
            visit += 1;
            let mut children = self
                .sessions
                .iter()
                .filter(|(_, record)| {
                    record.snapshot.session.parent == Some(spawner) && !record.owns_provider_actor()
                })
                .map(|(child_id, record)| (*child_id, record))
                .collect::<Vec<_>>();
            children.sort_by_key(|(child_id, record)| {
                (record.summary.created_at, child_id.to_string())
            });
            for (child_id, record) in children {
                tree.push(child_id);
                if let Some(identity) = record
                    .subagent_identity
                    .as_ref()
                    .filter(|identity| identity.provider == *provider)
                {
                    identified.push(IdentifiedSubagent {
                        session_id: child_id,
                        spawner,
                        subagent_id: &identity.subagent_id,
                    });
                }
            }
        }
        identified
    }
}

/// Which route spawned a Subagent (CONTEXT.md: Subagent), with what that
/// route fixes about its Session for good.
pub(super) enum SubagentRoute {
    /// Its spawner's own Provider spawned it, over the spawner's Provider
    /// actor, and named it by this identity.
    Native(StoredSubagentIdentity),
    /// Suru spawned it through the Broker, on a Provider actor of its own
    /// (ADR 0035). The Session id is its identity.
    Brokered,
}

/// What opening a Subagent's child Session takes beyond its spawner.
pub(super) struct ChildSession {
    pub(super) title: String,
    /// The Agent Selection it runs under, where its spawn chose one: a
    /// brokered Subagent's. A native Subagent's Model is known only once its
    /// Provider confirms one, so its Session selects nothing.
    pub(super) selection: Option<AgentSelection>,
    pub(super) approval_posture: Option<SessionApprovalPosture>,
    pub(super) delegation: Option<NormalizedText>,
    pub(super) route: SubagentRoute,
}

/// The row a stretch of a native Subagent's work stands as in the Turn that
/// delegated it, as the stretch opens — a spawn's or a resume's: working,
/// leading into the Subagent's own Session, and saying no Model until its
/// Provider confirms one and no duration until the stretch settles.
pub(crate) fn opening_subagent_row(
    turn_id: TurnId,
    name: String,
    description: String,
    session_id: SessionId,
) -> Activity {
    opening_row(turn_id, name, description, session_id, false)
}

/// The row a stretch of a brokered Subagent's work opens as: a native one's
/// in every way but the route it names, which is what lets a client offer its
/// stop whatever the delegating Agent's Provider allows (ADR 0035).
pub(super) fn opening_brokered_subagent_row(
    turn_id: TurnId,
    name: String,
    description: String,
    session_id: SessionId,
) -> Activity {
    opening_row(turn_id, name, description, session_id, true)
}

fn opening_row(
    turn_id: TurnId,
    name: String,
    description: String,
    session_id: SessionId,
    brokered: bool,
) -> Activity {
    Activity::Subagent {
        id: ActivityId::new(),
        turn_id,
        status: ActivityStatus::Active,
        name,
        description,
        model: None,
        session_id,
        brokered,
        duration_ms: None,
    }
}

/// A Session's stretches of work, oldest first: its Turns, less each
/// Continuation begun only to hold a Subagent's row — for a spawn or a resume
/// delegated after the Turn its delegating Agent worked in had settled (ADR
/// 0033, 0035). Such a Continuation is begun by no Prompt, settled at once,
/// and holds nothing but that row: its Agent did no work in it, so
/// whatever reads how a Subagent's work stands — its Marker and time in the
/// Subagent tree, what `read_subagent` answers — passes over it, as anything
/// that reports a stretch settling must.
pub(super) fn stretches_of_work(snapshot: &SessionSnapshot) -> Vec<&Turn> {
    let holding = snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .map(Activity::turn_id)
        .collect::<HashSet<_>>();
    if holding.is_empty() {
        return snapshot.turns.iter().collect();
    }
    let worked = snapshot
        .messages
        .iter()
        .map(|message| message.turn_id)
        .chain(
            snapshot
                .activities
                .iter()
                .filter(|activity| !matches!(activity, Activity::Subagent { .. }))
                .map(Activity::turn_id),
        )
        .collect::<HashSet<_>>();
    snapshot
        .turns
        .iter()
        .filter(|turn| {
            let only_holds_rows = turn.is_continuation()
                && turn.status.is_terminal()
                && holding.contains(&turn.id)
                && !worked.contains(&turn.id);
            !only_holds_rows
        })
        .collect()
}

/// A Subagent's Title: what its spawn described it doing, or its name where
/// the spawn described nothing.
pub(super) fn subagent_title(name: &str, description: &str) -> String {
    match description.trim() {
        "" => name.trim().to_owned(),
        described => described.to_owned(),
    }
}

impl SessionStoreState {
    /// Begins the next Turn in a Subagent's existing Session for a resume,
    /// under the store lock the caller holds: see
    /// [`SessionStore::begin_subagent_turn`].
    pub(super) fn begin_subagent_turn(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        delegation: Option<DeliveredDelegation>,
    ) -> anyhow::Result<TurnId> {
        let record = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        if !record.snapshot.session.is_subagent() {
            return Err(anyhow!(
                "Only a Subagent's Session begins a Turn on a resume"
            ));
        }
        let mut changes = Vec::new();
        if let Some(open) = active_turn_id(&record.snapshot)? {
            changes.extend(settle_in_flight_changes(
                &record.snapshot,
                open,
                TurnStatus::Completed,
                TrailingCommandOutput::new(),
                OpenInterventions::TurnEnded,
            ));
            changes.push(SessionChange::TurnStatusChanged {
                turn_id: open,
                status: TurnStatus::Completed,
                settled_at: None,
            });
        }
        let turn = Turn::unprompted(None);
        let turn_id = turn.id;
        changes.push(SessionChange::TurnAdded { turn });
        if let Some(delegation) = delegation {
            changes.push(SessionChange::MessageAdded {
                message: delegation_message(
                    turn_id,
                    delegator(&self.sessions, delegation.delegating_session),
                    delegation.text,
                ),
            });
        }
        self.commit(storage, session_id, changes)?;
        Ok(turn_id)
    }

    /// Opens a Subagent's child Session under `parent_id`, which the caller
    /// has found held: parented to it, in its Execution Directory, Workspace
    /// and checkout, and opened with the one prompt-less Turn the Subagent's
    /// first stretch of work runs in — headed by the spawn's Delegation, where
    /// there is one, as a Message from the parent's Agent. The child joins no
    /// listing and rides no catalog stream, so nothing is announced; it is
    /// reachable only through the row its spawner's Transcript shows, which
    /// the caller adds.
    pub(super) fn open_child_session(
        &mut self,
        storage: &StorageSink,
        parent_id: SessionId,
        child: ChildSession,
    ) -> SpawnedSubagentSession {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let timestamp = self.next_timestamp();
        let parent_session = &self
            .sessions
            .get(&parent_id)
            .expect("the caller found the parent held under the store lock")
            .snapshot
            .session;
        let delegation = child
            .delegation
            .map(|text| delegation_message(turn_id, delegator(&self.sessions, parent_id), text));
        let transcript = delegation
            .iter()
            .map(|message| TranscriptItem::Message {
                message_id: message.id,
            })
            .collect();
        let snapshot = SessionSnapshot {
            title: child.title.clone(),
            icon: None,
            session: Session {
                checkout: parent_session.checkout.clone(),
                context_fill: None,
                id: session_id,
                execution_directory: parent_session.execution_directory.clone(),
                workspace: parent_session.workspace.clone(),
                // A selection a brokered spawn chose was checked against the
                // Model Catalog as it spawned; a native Subagent's Session
                // selects nothing and reads as its parent's does.
                agent_selection_availability: if child.selection.is_some() {
                    ModelAvailability::Available
                } else {
                    parent_session.agent_selection_availability
                },
                agent_selection: child.selection,
                approval_posture: child.approval_posture,
                status: SessionStatus::Active,
                working_since: Some(timestamp),
                monitoring_since: None,
                parent: Some(parent_id),
                begun_by: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: vec![Turn {
                id: turn_id,
                // Stamped here rather than by a commit, because the spawn is
                // the child's creation and no commit delivers its Turn.
                started_at: Some(timestamp),
                ..Turn::unprompted(None)
            }],
            messages: delegation.into_iter().collect(),
            activities: Vec::new(),
            transcript,
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            waiting_on_subagents: None,
            subagent_usage: None,
            total_cost: None,
            own_cost: None,
            attachments: Vec::new(),
        };
        let summary = SessionSummary {
            checkout_state: None,
            session: snapshot.session.clone(),
            title: child.title,
            // A child is titled from its spawn alone: no Errand derives it a
            // Title, so no Icon ever arrives beside one.
            icon: None,
            settled_at: None,
            standing_inputs: SessionStandingInputs::from_turns(&snapshot.turns),
            total_usage: snapshot.total_usage(),
            own_cost: snapshot.own_cost,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let (subagent_identity, brokered) = match child.route {
            SubagentRoute::Native(identity) => (Some(identity), false),
            SubagentRoute::Brokered => (None, true),
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        storage.created(
            PersistedSession {
                subagent_identity: subagent_identity.clone(),
                brokered,
                ..PersistedSession::created(summary.clone(), snapshot.clone())
            },
            Vec::new(),
        );
        self.sessions.insert(
            session_id,
            SessionRecord {
                context_fill_order: None,
                snapshot,
                summary,
                updates,
                next_prompt_order: PromptOrder(1),
                steer_targets: HashMap::new(),
                turn_start_admissions: Default::default(),
                selection_operations: HashMap::new(),
                viewed_operations: Default::default(),
                selection_retry_prompt: None,
                resume_states: HashMap::new(),
                subagent_identity,
                brokered,
                watches: HashMap::new(),
                subagent_waits: Vec::new(),
                work_interrupted_at: None,
                stopped_by_ancestor: None,
                held_reports: Default::default(),
                acts_to_store: Vec::new(),
                sidekicks_owed: Vec::new(),
            },
        );
        // The child begins working the moment it exists, which the listed
        // root's Working reading has to carry. Its total is derived on the
        // same terms, so both readings above it are answered from the same
        // subtree — a child that has consumed nothing yet moves neither.
        self.reconcile_liveness(storage, session_id);
        self.reconcile_usage(storage, session_id);
        SpawnedSubagentSession {
            session_id,
            turn_id,
        }
    }
}

/// Names the Agent of `session_id` the way a Delegation it sent names it: by
/// its Session, and — when that is a Subagent's — by the name the Subagent's
/// rows carry in its spawner's Transcript.
pub(super) fn delegator(
    sessions: &HashMap<SessionId, SessionRecord>,
    session_id: SessionId,
) -> Delegator {
    let name = sessions
        .get(&session_id)
        .and_then(|record| record.snapshot.session.parent)
        .and_then(|spawner| sessions.get(&spawner))
        .and_then(|spawner| {
            spawner
                .snapshot
                .activities
                .iter()
                .find_map(|activity| match activity {
                    Activity::Subagent {
                        session_id: child,
                        name,
                        ..
                    } if *child == session_id => Some(name.clone()),
                    _ => None,
                })
        });
    Delegator { session_id, name }
}

/// The Message a Delegation stands as in the Turn it opens or steers. It
/// arrives whole, so it is complete from the start.
pub(super) fn delegation_message(
    turn_id: TurnId,
    delegator: Delegator,
    text: NormalizedText,
) -> Message {
    Message {
        id: MessageId::new(),
        turn_id,
        role: MessageRole::Delegation(delegator),
        status: MessageStatus::Completed,
        content: text.content,
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        truncated: text.truncated,
        author: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{AgentSelection, ModelId, ProviderId, SessionId};
    use crate::provider::ProviderSubagentId;
    use crate::sessions::{SessionStore, restoration_tests::persisted};
    use crate::storage::{
        PersistedSession, RestoredSessions, StorageRepository, StorageWriter,
        StoredSubagentIdentity,
    };

    /// A store holding `readable` as the process that restored them would,
    /// with its writer so the test can stop it.
    async fn restored(
        directory: &std::path::Path,
        readable: Vec<PersistedSession>,
    ) -> (SessionStore, StorageWriter) {
        let repository = StorageRepository::open(directory).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(
            RestoredSessions {
                readable,
                ..Default::default()
            },
            sink,
            Vec::new(),
            Default::default(),
        );
        (store, writer)
    }

    /// A Session that owns a Provider actor on `provider` — a top-level
    /// Session with no `parent`, or a brokered Subagent's beneath one — as
    /// the Session a Broker token names always does.
    fn owning(
        workspace: &std::path::Path,
        parent: Option<SessionId>,
        provider: &str,
    ) -> PersistedSession {
        let mut session = PersistedSession {
            brokered: parent.is_some(),
            ..persisted(workspace, parent)
        };
        let selection = Some(AgentSelection {
            provider: ProviderId::new(provider),
            model: ModelId::new("model"),
            options: Vec::new(),
        });
        session.snapshot.session.agent_selection = selection.clone();
        session.summary.session.agent_selection = selection;
        session
    }

    /// A native Subagent's Session beneath `parent`, which `provider` knows
    /// by `thread`.
    fn native(
        workspace: &std::path::Path,
        parent: SessionId,
        provider: &str,
        thread: &str,
    ) -> PersistedSession {
        PersistedSession {
            subagent_identity: Some(StoredSubagentIdentity {
                provider: ProviderId::new(provider),
                subagent_id: ProviderSubagentId::new(thread),
            }),
            ..persisted(workspace, Some(parent))
        }
    }

    fn id(session: &PersistedSession) -> SessionId {
        session.snapshot.session.id
    }

    /// A Broker call's metadata may name the native Subagent making it, and
    /// is trusted only as far as the Provider connection the call's token
    /// was handed: to a native Subagent riding that connection, at any depth,
    /// by the identity that connection's Provider minted.
    #[tokio::test]
    async fn a_native_subagent_is_found_by_its_identity_only_on_the_connection_it_rides() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let top_level = owning(workspace, None, "codex");
        let child = native(workspace, id(&top_level), "codex", "child-thread");
        let grandchild = native(workspace, id(&child), "codex", "grandchild-thread");
        let foreign = native(workspace, id(&top_level), "claude", "foreign-thread");
        let brokered = owning(workspace, Some(id(&child)), "codex");
        let brokereds_child = native(workspace, id(&brokered), "codex", "brokered-child-thread");
        let stranger = owning(workspace, None, "codex");
        let strangers_child = native(workspace, id(&stranger), "codex", "stranger-thread");
        let (top_level_id, child_id, grandchild_id, brokered_id, brokereds_child_id) = (
            id(&top_level),
            id(&child),
            id(&grandchild),
            id(&brokered),
            id(&brokereds_child),
        );
        let (store, writer) = restored(
            workspace,
            vec![
                top_level,
                child,
                grandchild,
                foreign,
                brokered,
                brokereds_child,
                stranger,
                strangers_child,
            ],
        )
        .await;
        let riding = |owner: SessionId, thread: &str| {
            store.native_subagent_riding(owner, &ProviderSubagentId::new(thread))
        };

        assert_eq!(riding(top_level_id, "child-thread"), Some(child_id));
        assert_eq!(
            riding(top_level_id, "grandchild-thread"),
            Some(grandchild_id),
            "a native Subagent's own native Subagent rides the same connection"
        );
        assert_eq!(
            riding(brokered_id, "brokered-child-thread"),
            Some(brokereds_child_id),
            "a brokered Subagent's connection knows the native Subagents riding it"
        );
        for (thread, says) in [
            (
                "brokered-child-thread",
                "a native Subagent riding a brokered Subagent's own connection",
            ),
            ("stranger-thread", "another tree's native Subagent"),
            ("foreign-thread", "an identity another Provider minted"),
            ("never-spawned", "an identity nothing carries"),
        ] {
            assert_eq!(
                riding(top_level_id, thread),
                None,
                "the top-level Session's connection never names {says}"
            );
        }
        writer.shutdown().await.unwrap();
    }
}
