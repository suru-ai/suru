//! Subagent Reports waiting for the Agent they are for (ADR 0035).
//!
//! A brokered Subagent's Report is built where its row settles (see
//! [`SessionStoreState::follow_brokered_turns`]) and held here, against the
//! Session whose Agent delegated that stretch of work, until Provider
//! orchestration hands it to that Agent's Provider: at once — steering a Turn
//! at work, or waking an idle Agent into a Continuation — or at the head of
//! the Session's next Turn when no Provider process is running to take it.
//! Suru never relaunches a Provider to deliver one. A Report its Provider
//! takes has its Agent working again, so a Session the user had set aside is
//! brought back once one is delivered — never while one only waits.
//!
//! Nothing here reaches a Transcript: a Report stands in none.

use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::protocol::{AgentIdentity, SessionChange, SessionId, Turn, TurnId};
use crate::provider::SubagentReport;

use super::{SessionStore, SessionStoreState, projection::active_turn_id};

impl SessionStore {
    /// Asks to hear, by the Session it is for, of every Subagent Report that
    /// comes to wait in the store from here on, so the Report can be handed
    /// to that Session's Agent. Reports already waiting are not announced
    /// again: whatever begins that Session's next Turn takes them.
    pub(crate) fn announce_held_reports_to(&self, notices: mpsc::UnboundedSender<SessionId>) {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .report_notices = Some(notices);
    }

    /// Takes every Report waiting for `session_id`, oldest first, for the
    /// Provider input about to carry them: the head of a Turn beginning, or a
    /// steer of the Turn at work.
    pub(crate) fn take_held_reports(&self, session_id: SessionId) -> Vec<SubagentReport> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get_mut(&session_id)
            .map(|record| record.held_reports.drain(..).collect())
            .unwrap_or_default()
    }

    /// Puts `reports` — taken for input its Provider never took — back where
    /// they waited for `session_id`, ahead of any that came to wait since, so
    /// the next input that reaches the Agent carries them in the order they
    /// arrived.
    pub(crate) fn hold_reports_again(&self, session_id: SessionId, reports: Vec<SubagentReport>) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get_mut(&session_id) else {
            return;
        };
        for report in reports.into_iter().rev() {
            record.held_reports.push_front(report);
        }
    }

    /// Brings `session_id` back from being set aside — and every Session above
    /// it, since the one a tree is listed by is its top-level Session — now
    /// that its Provider has taken Subagent Reports for its Agent, which is
    /// working on them: waking into a Continuation, steered, or beginning its
    /// next Turn with them at its head (CONTEXT.md: Subagent Report). Brought
    /// back exactly as the user bringing it back would, so its last activity
    /// is now and auto-settle leaves it standing.
    pub(crate) fn reports_delivered(&self, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let lineage = state
            .ancestors(session_id)
            .map(|(session_id, _)| session_id)
            .collect::<Vec<_>>();
        for session_id in lineage {
            state.set_settled(&self.storage, session_id, false, None);
        }
    }

    /// Begins the Continuation the Reports waiting for `session_id` wake its
    /// idle Agent into — run by `agent`, begun by neither a Prompt nor a
    /// Delegation — and takes those Reports as the whole of its input, in the
    /// one lock, so no Report can slip between the Turn and its input. `None`
    /// when no Report waits, or when a Turn is already open in the Session: a
    /// Delegation's, opened ahead of its delivery, whose input takes them.
    pub(crate) fn begin_report_continuation(
        &self,
        session_id: SessionId,
        agent: AgentIdentity,
    ) -> anyhow::Result<Option<(TurnId, Vec<SubagentReport>)>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        if record.held_reports.is_empty() || active_turn_id(&record.snapshot)?.is_some() {
            return Ok(None);
        }
        let turn = Turn::unprompted(Some(agent));
        let turn_id = turn.id;
        state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::TurnAdded { turn }],
        )?;
        let reports = state
            .sessions
            .get_mut(&session_id)
            .map(|record| record.held_reports.drain(..).collect())
            .unwrap_or_default();
        Ok(Some((turn_id, reports)))
    }
}

impl SessionStoreState {
    /// Holds `report` for `recipient`'s Agent and says so to whoever asked to
    /// hear. Held, the Report leaves a Session the user set aside where it
    /// stands: nothing is working on it until a Provider takes it (see
    /// [`SessionStore::reports_delivered`]).
    pub(super) fn hold_report(&mut self, recipient: SessionId, report: SubagentReport) {
        let Some(record) = self.sessions.get_mut(&recipient) else {
            return;
        };
        record.held_reports.push_back(report);
        if let Some(notices) = &self.report_notices
            && notices.send(recipient).is_err()
        {
            // Whoever asked has stopped listening, as a stopping server's
            // orchestrator does: the Report waits for the next Turn.
            self.report_notices = None;
        }
    }
}
