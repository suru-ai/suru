//! Opening a Subagent's child Session — the one Session creation a Prompt does
//! not drive — beginning each later Turn a resume of the Subagent begins in
//! it, each opened by the Delegation that began it, and finding the Session a
//! resume continues by the identity its Provider stored with it at the spawn.

use std::collections::HashMap;

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, Delegator, Message, MessageId, MessageRole, MessageStatus, PromptOrder, ProviderId,
    Session, SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStandingInputs,
    SessionStatus, SessionSummary, TranscriptItem, Turn, TurnId, TurnStatus,
};
use crate::provider::ProviderSubagentId;
use crate::storage::StoredSubagentIdentity;

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore,
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

/// The Delegation a resume's Turn opens with: what the delegating Agent
/// asked, already normalized and capped the way an Agent Message's content
/// is, and the Session whose Agent sent it — the Subagent's parent, or a
/// sibling Subagent's.
pub(crate) struct OpeningDelegation {
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
        if !state.sessions.contains_key(&parent_id) {
            return Err(anyhow!("Session does not exist on this server instance"));
        }
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let timestamp = state.next_timestamp();
        let parent = state
            .sessions
            .get(&parent_id)
            .expect("Session existence was checked while holding the store lock");
        let parent_session = &parent.snapshot.session;
        let title = match description.trim() {
            "" => name.trim().to_owned(),
            described => described.to_owned(),
        };
        let delegation = delegation
            .map(|text| delegation_message(turn_id, delegator(&state.sessions, parent_id), text));
        let transcript = delegation
            .iter()
            .map(|message| TranscriptItem::Message {
                message_id: message.id,
            })
            .collect();
        let snapshot = SessionSnapshot {
            title: title.clone(),
            icon: None,
            session: Session {
                checkout: parent_session.checkout.clone(),
                context_fill: None,
                id: session_id,
                execution_directory: parent_session.execution_directory.clone(),
                workspace: parent_session.workspace.clone(),
                agent_selection: None,
                agent_selection_availability: parent_session.agent_selection_availability,
                approval_posture: parent_session.approval_posture,
                status: SessionStatus::Active,
                working_since: Some(timestamp),
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
            subagent_usage: None,
            total_cost: None,
        };
        let summary = SessionSummary {
            checkout_state: None,
            session: snapshot.session.clone(),
            title,
            // A child is titled from its spawn description alone: no Errand
            // derives it a Title, so no Icon ever arrives beside one.
            icon: None,
            settled_at: None,
            standing_inputs: SessionStandingInputs::from_turns(&snapshot.turns),
            total_usage: snapshot.total_usage(),
            created_at: timestamp,
            updated_at: timestamp,
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        let persisted_summary = summary.clone();
        state.sessions.insert(
            session_id,
            SessionRecord {
                context_fill_order: None,
                snapshot: snapshot.clone(),
                summary,
                updates,
                next_prompt_order: PromptOrder(1),
                steer_targets: HashMap::new(),
                turn_start_admissions: Default::default(),
                selection_operations: HashMap::new(),
                viewed_operations: Default::default(),
                selection_retry_prompt: None,
                resume_states: HashMap::new(),
                subagent_identity: Some(identity.clone()),
            },
        );
        self.storage
            .created(persisted_summary, snapshot, Some(identity));
        // The child begins working the moment it exists, which the listed
        // root's Working reading has to carry. Its total is derived on the
        // same terms, so both readings above it are answered from the same
        // subtree — a child that has consumed nothing yet moves neither.
        state.reconcile_working(&self.storage, session_id);
        state.reconcile_usage(&self.storage, session_id);
        Ok(SpawnedSubagentSession {
            session_id,
            turn_id,
        })
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
        delegation: Option<OpeningDelegation>,
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

    /// Every Subagent in the tree `top_level` heads whose Session the store
    /// holds by an identity `provider` minted, nearest the top first. This is
    /// what a Provider connection that resumes its conversation — after a
    /// restart above all — relearns its Subagents from: the identities are the
    /// connection's own, and the connection belongs to the tree's top-level
    /// Session, so no identity here can resolve into another tree.
    pub(crate) fn stored_subagents(
        &self,
        top_level: SessionId,
        provider: &ProviderId,
    ) -> Vec<StoredSubagent> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut tree = vec![top_level];
        let mut stored = Vec::new();
        let mut visit = 0;
        while visit < tree.len() {
            let spawner = tree[visit];
            visit += 1;
            let mut children = state
                .sessions
                .iter()
                .filter(|(_, record)| record.snapshot.session.parent == Some(spawner))
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

/// Names the Agent of `session_id` the way a Delegation it sent names it: by
/// its Session, and — when that is a Subagent's — by the name the Subagent's
/// rows carry in its spawner's Transcript.
fn delegator(sessions: &HashMap<SessionId, SessionRecord>, session_id: SessionId) -> Delegator {
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

/// The Message a Delegation stands as in the Turn it opens. It arrives whole,
/// so it is complete from the start.
fn delegation_message(turn_id: TurnId, delegator: Delegator, text: NormalizedText) -> Message {
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
