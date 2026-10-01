//! The Server's shared record of when any Client last had a Session open.

use crate::protocol::{SessionCatalogChange, SessionId, SessionSummary, ViewSessionRequest};

use super::SessionStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewSessionError {
    SessionNotFound,
    SubagentSession,
}

impl SessionStore {
    /// Stamps the Server's clock as the Session's Viewed moment and announces
    /// the complete Standing input to every catalog subscriber.
    pub(crate) fn view(
        &self,
        session_id: SessionId,
        request: ViewSessionRequest,
    ) -> Result<SessionSummary, ViewSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get(&session_id) else {
            return Err(ViewSessionError::SessionNotFound);
        };
        if record.snapshot.session.is_subagent() {
            return Err(ViewSessionError::SubagentSession);
        }
        if record.viewed_operations.contains(&request.operation_id) {
            return Ok(record.summary.clone());
        }
        let viewed_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.summary.standing_inputs.viewed_at = Some(viewed_at);
        record.viewed_operations.insert(request.operation_id);
        let summary = record.summary.clone();
        self.storage.summary_changed(summary.clone(), Vec::new());
        state.publish_catalog_change(SessionCatalogChange::StandingInputsChanged {
            session_id,
            inputs: summary.standing_inputs.clone(),
        });
        Ok(summary)
    }
}
