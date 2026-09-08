//! Opening a Subagent's child Session: the one Session creation a Prompt does
//! not drive.

use std::collections::HashMap;

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::protocol::{
    AgentIdentity, PromptOrder, Session, SessionId, SessionRevision, SessionSnapshot,
    SessionStandingInputs, SessionStatus, SessionSummary, Turn, TurnId, TurnStatus,
};

use super::{SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore};

/// The child Session a spawn opened, named by what orchestration needs to
/// route the Subagent's stream: the Session itself, and the prompt-less Turn
/// its attributed events land in.
pub(crate) struct SpawnedSubagentSession {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
}

impl SessionStore {
    /// Creates the child Session a Subagent runs in: parented to its spawner,
    /// titled from the spawn description — never an Errand — and opened with
    /// the one prompt-less Turn its attributed events land in. The child joins
    /// no listing and rides no catalog stream, so nothing is announced; it is
    /// reachable only through the row its spawner's Transcript shows.
    pub(crate) fn create_subagent(
        &self,
        parent_id: SessionId,
        agent: Option<AgentIdentity>,
        name: &str,
        description: &str,
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
        let snapshot = SessionSnapshot {
            title: title.clone(),
            emoji: None,
            session: Session {
                context_fill: None,
                id: session_id,
                execution_directory: parent_session.execution_directory.clone(),
                workspace: parent_session.workspace.clone(),
                agent_selection: parent_session.agent_selection.clone(),
                agent_selection_availability: parent_session.agent_selection_availability,
                status: SessionStatus::Active,
                working_since: Some(timestamp),
                parent: Some(parent_id),
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: vec![Turn {
                id: turn_id,
                prompt_id: None,
                agent,
                status: TurnStatus::Active,
                // Stamped here rather than by a commit, because the spawn is
                // the child's creation and no commit delivers its Turn.
                started_at: Some(timestamp),
                settled_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
            }],
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            subagent_questionnaires: Vec::new(),
            subagent_usage: None,
        };
        let summary = SessionSummary {
            session: snapshot.session.clone(),
            title,
            // A child is titled from its spawn description alone: no Errand
            // derives it a Title, so no Emoji ever arrives beside one.
            emoji: None,
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
                pending_turn_starts: Default::default(),
                selection_operations: HashMap::new(),
                viewed_operations: Default::default(),
                selection_retry_prompt: None,
                resume_states: HashMap::new(),
            },
        );
        self.storage.created(persisted_summary, snapshot);
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
}
