//! The Turn a Compaction request begins (ADR 0041).
//!
//! Only an idle Session takes the request, and idle is Suru's own reading
//! rather than the Provider's: a Provider asked to compact mid-Turn may abort
//! the Turn, or report success and lose the Compaction. So the reading and the
//! Turn it opens are one act under the store lock, and any Prompt admitted
//! after it finds the Session Working. The Provider can still begin a turn of
//! its own — a Watch waking its Agent — before it is asked to compact. Then
//! the request gives way: its Turn fails saying so, and the native turn takes
//! the Continuation it would have begun.
//!
//! The Turn takes no steer, so a Prompt admitted while it runs is held behind
//! it, owed the next Turn. Its writer expects that Turn to begin on a
//! compacted context: the Prompt begins it once the Compaction completes, and
//! is withdrawn as the Turn settles any other way (ADR 0024).

use crate::{
    protocol::{
        AgentIdentity, PromptStatus, SessionChange, SessionId, SessionSnapshot, Turn, TurnId,
        TurnStatus,
    },
    provider::ManualCompactionRefusal,
};

use super::{
    OpenInterventions, SessionRecord, SessionStore, TrailingCommandOutput,
    projection::{active_turn_id, undelivered_turn_starts},
    settlement::fail_turn_changes,
    subagent_tree::needs_intervention,
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

    /// Fails the Turn a Compaction request opened in `session_id` whose
    /// Provider has not yet been asked to compact, because the Provider began
    /// a turn of its own first — a Watch waking its Agent, say. Writing
    /// `/compact` into a loop already running would compact under work it
    /// cuts off, so the request gives way, saying why in `message`, and the
    /// Turn it failed is answered for the caller to drop its request. A
    /// Session with no such Turn open answers nothing.
    ///
    /// Only the Provider actor that has not yet asked for the Compaction may
    /// call this: once it has, the Turn is that Compaction's, at work.
    pub(crate) fn withdraw_unasked_compaction(
        &self,
        session_id: SessionId,
        message: String,
    ) -> anyhow::Result<Option<TurnId>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get(&session_id) else {
            return Ok(None);
        };
        let Some(turn_id) = active_turn_id(&record.snapshot)?.filter(|turn_id| {
            record
                .snapshot
                .turns
                .iter()
                .any(|turn| turn.id == *turn_id && turn.compaction_requested)
        }) else {
            return Ok(None);
        };
        let mut changes = fail_turn_changes(
            &record.snapshot,
            turn_id,
            TrailingCommandOutput::new(),
            message,
            None,
            OpenInterventions::TurnEnded,
        );
        changes.extend(record.held_prompt_withdrawals(turn_id, TurnStatus::Failed));
        state.commit(&self.storage, session_id, changes)?;
        Ok(Some(turn_id))
    }
}

impl SessionRecord {
    /// The withdrawal of every Prompt held behind `turn_id`, owed to its
    /// settling as `status`. Only an active Turn a Compaction request began
    /// holds any: it takes no steer, so each Prompt admitted to begin a Turn
    /// while it runs waits behind it — and those are the only Prompts it can
    /// owe a Turn, since the request was taken only while it owed none. A
    /// Turn settling as completed leaves them to begin the next Turn on the
    /// context it compacted; any other settling withdraws them in the same
    /// commit (ADR 0024), so none reaches a context its writer expected
    /// compacted, and the Session stops Working with the Turn.
    pub(super) fn held_prompt_withdrawals(
        &self,
        turn_id: TurnId,
        status: TurnStatus,
    ) -> Vec<SessionChange> {
        let holds_prompts = self.snapshot.turns.iter().any(|turn| {
            turn.id == turn_id && turn.compaction_requested && turn.status == TurnStatus::Active
        });
        if !holds_prompts || status == TurnStatus::Completed {
            return Vec::new();
        }
        undelivered_turn_starts(&self.snapshot, &self.turn_start_admissions)
            .map(|prompt| SessionChange::PromptStatusChanged {
                prompt_id: prompt.id,
                status: PromptStatus::Cancelled,
            })
            .collect()
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
