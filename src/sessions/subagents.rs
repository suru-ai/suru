//! Opening a Subagent's child Session — the one Session creation a Prompt does
//! not drive, whichever route spawned the Subagent — beginning each later Turn
//! a resume of the Subagent begins in it, each opened by the Delegation that
//! began it, adding each Delegation that steers a Turn still working, and
//! finding the Session a resume continues by the identity its Provider stored
//! with it at the spawn.

use std::collections::HashMap;

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, AgentSelection, Delegator, Message, MessageId, MessageRole, MessageStatus,
    ModelAvailability, PromptOrder, ProviderId, Session, SessionApprovalPosture, SessionChange,
    SessionId, SessionRevision, SessionSnapshot, SessionStandingInputs, SessionStatus,
    SessionSummary, TranscriptItem, Turn, TurnId, TurnStatus,
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
                "Only a Subagent's Session begins a Turn on a resume"
            ));
        }
        let mut changes = Vec::new();
        if let Some(open) = active_turn_id(&record.snapshot)? {
            changes.extend(settle_in_flight_changes(
                &record.snapshot,
                open,
                TrailingCommandOutput::new(),
                OpenInterventions::TurnEnded,
            ));
            changes.push(SessionChange::TurnStatusChanged {
                turn_id: open,
                status: TurnStatus::Completed,
                settled_at: None,
            });
        }
        let turn_id = TurnId::new();
        changes.push(SessionChange::TurnAdded {
            turn: Turn {
                id: turn_id,
                prompt_id: None,
                agent: None,
                status: TurnStatus::Active,
                // The commit that lands this Turn stamps when it began.
                started_at: None,
                settled_at: None,
                last_output_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
                cost_details: None,
            },
        });
        if let Some(delegation) = delegation {
            changes.push(SessionChange::MessageAdded {
                message: delegation_message(
                    turn_id,
                    delegator(&state.sessions, delegation.delegating_session),
                    delegation.text,
                ),
            });
        }
        state.commit(&self.storage, session_id, changes)?;
        Ok(turn_id)
    }

    /// Adds a steer to the Turn a Subagent is working in (ADR 0032): the
    /// Delegation stands as a Message from the delegating Agent after
    /// everything the Subagent did before it received it, and begins no Turn.
    /// A Turn that has Settled accepts no further Delegation, since a steer
    /// is only ever delivered into work still going.
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
        let mut tree = vec![owner];
        let mut stored = Vec::new();
        let mut visit = 0;
        while visit < tree.len() {
            let spawner = tree[visit];
            visit += 1;
            let mut children = state
                .sessions
                .iter()
                .filter(|(_, record)| {
                    record.snapshot.session.parent == Some(spawner) && !record.owns_provider_actor()
                })
                .map(|(child_id, record)| (*child_id, record))
                .collect::<Vec<_>>();
            // Spawn order, so an identity a Provider ever named twice resolves
            // to the Subagent that first carried it.
            children.sort_by_key(|(child_id, record)| {
                (record.summary.created_at, child_id.to_string())
            });
            for (child_id, record) in children {
                tree.push(child_id);
                let Some(identity) = record
                    .subagent_identity
                    .as_ref()
                    .filter(|identity| identity.provider == *provider)
                else {
                    continue;
                };
                // The name the spawn's row carries in the spawner's Transcript.
                let name = delegator(&state.sessions, child_id)
                    .name
                    .unwrap_or_default();
                stored.push(StoredSubagent {
                    subagent_id: identity.subagent_id.clone(),
                    session_id: child_id,
                    spawner,
                    name,
                });
            }
        }
        stored
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

/// A Subagent's Title: what its spawn described it doing, or its name where
/// the spawn described nothing.
pub(super) fn subagent_title(name: &str, description: &str) -> String {
    match description.trim() {
        "" => name.trim().to_owned(),
        described => described.to_owned(),
    }
}

impl SessionStoreState {
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
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: vec![Turn {
                id: turn_id,
                prompt_id: None,
                agent: None,
                status: TurnStatus::Active,
                // Stamped here rather than by a commit, because the spawn is
                // the child's creation and no commit delivers its Turn.
                started_at: Some(timestamp),
                settled_at: None,
                last_output_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
                cost_details: None,
            }],
            messages: delegation.into_iter().collect(),
            activities: Vec::new(),
            transcript,
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            subagent_usage: None,
            total_cost: None,
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
            created_at: timestamp,
            updated_at: timestamp,
        };
        let (subagent_identity, brokered) = match child.route {
            SubagentRoute::Native(identity) => (Some(identity), false),
            SubagentRoute::Brokered => (None, true),
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        storage.created(PersistedSession {
            subagent_identity: subagent_identity.clone(),
            brokered,
            ..PersistedSession::created(summary.clone(), snapshot.clone())
        });
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
        truncated: text.truncated,
    }
}
