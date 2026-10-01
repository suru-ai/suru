//! The Turn a Compaction request begins (ADR 0041).
//!
//! Only an idle Session takes the request, and idle is Suru's own reading
//! rather than the Provider's: a Provider asked to compact mid-Turn may abort
//! the Turn, or report success and lose the Compaction. So the reading and the
//! Turn it opens are one act under the store lock, and any Prompt admitted
//! after it finds the Session Working.

use crate::{
    protocol::{AgentIdentity, SessionChange, SessionId, SessionSnapshot, Turn, TurnId},
    provider::ManualCompactionRefusal,
};

use super::{
    SessionRecord, SessionStore, projection::active_turn_id, subagent_tree::needs_intervention,
};

/// Why a Session refused to begin a Turn for a Compaction request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactSessionError {
    SessionNotFound,
    /// The Session is a Subagent's, whose context its Provider alone decides
    /// to compact.
    SubagentSession,
    /// The Session's Provider compacts only when it chooses to.
    Unsupported,
    /// The request carried instructions for the summary, and the Session's
    /// Provider takes none.
    InstructionsUnsupported,
    /// The Session owes its reader an Intervention, its own or one of a
    /// Subagent below it.
    PendingIntervention,
    /// The Session is Working: running a Turn, owing one to a Prompt it
    /// admitted, or waiting on Subagents still at work below it.
    WorkingSession,
}

impl From<ManualCompactionRefusal> for CompactSessionError {
    fn from(refusal: ManualCompactionRefusal) -> Self {
        match refusal {
            ManualCompactionRefusal::Unsupported => Self::Unsupported,
            ManualCompactionRefusal::InstructionsUnsupported => Self::InstructionsUnsupported,
        }
    }
}

impl SessionStore {
    /// Begins the Turn a Compaction request opens in `session_id`, whose only
    /// content will be the Compaction its Provider reports, and answers its
    /// identity. The Session must be a top-level one owing its reader no
    /// Intervention and Working at nothing, which is read and acted on under
    /// the one lock, so the Turn it opens is what any later admission finds.
    /// Work arriving for the Session takes it off the shelf, as a Prompt does.
    ///
    /// The Turn runs under the Session's own Agent Selection, as a Prompt's
    /// would. The Provider's identity for its Agent is the one the Session's
    /// earlier Turns ran under, since a Session keeps its Provider (ADR
    /// 0005); a Session no Turn has yet bound runs it unbound.
    pub(crate) fn begin_requested_compaction(
        &self,
        session_id: SessionId,
    ) -> Result<TurnId, CompactSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get_mut(&session_id)
            .ok_or(CompactSessionError::SessionNotFound)?;
        if record.snapshot.session.is_subagent() {
            return Err(CompactSessionError::SubagentSession);
        }
        if owes_an_intervention(record) {
            return Err(CompactSessionError::PendingIntervention);
        }
        if is_working(&record.snapshot) {
            return Err(CompactSessionError::WorkingSession);
        }
        let turn = Turn::requested_compaction(requested_agent(&record.snapshot));
        let turn_id = turn.id;
        let reactivation = record.reactivate(session_id);
        state
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::TurnAdded { turn }],
            )
            .expect("opening a Turn on an idle Session preserves its invariants");
        if let Some(change) = reactivation {
            state.publish_catalog_change(change);
        }
        Ok(turn_id)
    }
}

/// Whether the Session owes its reader a Decision or an Answer, its own or a
/// Subagent's below it — the readings its Standing counts as Needs
/// Intervention.
fn owes_an_intervention(record: &SessionRecord) -> bool {
    needs_intervention(record)
        || record.snapshot.subagent_interventions.iter().any(|owed| {
            !owed.pending_approvals.is_empty() || !owed.pending_questionnaires.is_empty()
        })
}

/// Whether the Session is Working at anything, below it included: its own
/// Turn, a Prompt owed one, or Subagents working on past the Turn that
/// spawned them.
fn is_working(snapshot: &SessionSnapshot) -> bool {
    snapshot.session.working_since.is_some()
        || active_turn_id(snapshot)
            .expect("stored Sessions preserve the one-active-Turn invariant")
            .is_some()
}

/// The Agent a Compaction request's Turn runs under: the Provider's identity
/// for it from the Session's latest bound Turn, under the Session's own Agent
/// Selection where it has one.
fn requested_agent(snapshot: &SessionSnapshot) -> Option<AgentIdentity> {
    let previous = snapshot
        .turns
        .iter()
        .rev()
        .find_map(|turn| turn.agent.as_ref())?;
    Some(AgentIdentity {
        agent: previous.agent.clone(),
        selection: snapshot
            .session
            .agent_selection
            .clone()
            .unwrap_or_else(|| previous.selection.clone()),
    })
}
