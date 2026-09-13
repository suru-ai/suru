//! Session Approval Posture overrides and live Server Setting reconciliation.

use crate::protocol::{
    ApprovalPosture, EffectiveSettings, SessionApprovalPosture, SessionChange, SessionId,
    UpdateApprovalPostureRequest,
};

use super::SessionStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPostureMutationError {
    SessionNotFound,
    ProviderUnavailable,
    ProviderConflict,
    Storage,
}

impl SessionStore {
    pub(crate) fn apply_approval_posture_command(
        &self,
        session_id: SessionId,
        request: UpdateApprovalPostureRequest,
        settings: &EffectiveSettings,
    ) -> Result<SessionApprovalPosture, ApprovalPostureMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let selection = state
            .sessions
            .get(&session_id)
            .ok_or(ApprovalPostureMutationError::SessionNotFound)?
            .snapshot
            .session
            .agent_selection
            .clone()
            .ok_or(ApprovalPostureMutationError::ProviderUnavailable)?;
        let value = match request.posture {
            Some(posture) => {
                if posture.provider() != selection.provider {
                    return Err(ApprovalPostureMutationError::ProviderConflict);
                }
                SessionApprovalPosture {
                    value: posture,
                    pinned: true,
                }
            }
            None => SessionApprovalPosture {
                value: ApprovalPosture::for_provider(&selection.provider, settings)
                    .ok_or(ApprovalPostureMutationError::ProviderUnavailable)?,
                pinned: false,
            },
        };
        if state.sessions[&session_id]
            .snapshot
            .session
            .approval_posture
            .as_ref()
            != Some(&value)
        {
            state
                .commit(
                    &self.storage,
                    session_id,
                    vec![SessionChange::ApprovalPostureChanged {
                        approval_posture: Some(value.clone()),
                    }],
                )
                .map_err(|_| ApprovalPostureMutationError::Storage)?;
        }
        Ok(value)
    }

    /// Refreshes the effective reading of every loaded, unpinned Session.
    /// Each change is a normal durable revision so every local or remote
    /// viewer observes the same authoritative posture.
    pub(crate) fn reconcile_approval_postures(&self, settings: &EffectiveSettings) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let ids = state.sessions.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let next = state.sessions.get(&id).and_then(|record| {
                if record
                    .snapshot
                    .session
                    .approval_posture
                    .as_ref()
                    .is_some_and(|posture| posture.pinned)
                {
                    return None;
                }
                let provider = &record.snapshot.session.agent_selection.as_ref()?.provider;
                Some(SessionApprovalPosture {
                    value: ApprovalPosture::for_provider(provider, settings)?,
                    pinned: false,
                })
            });
            let Some(next) = next else { continue };
            if state.sessions[&id]
                .snapshot
                .session
                .approval_posture
                .as_ref()
                == Some(&next)
            {
                continue;
            }
            if let Err(error) = state.commit(
                &self.storage,
                id,
                vec![SessionChange::ApprovalPostureChanged {
                    approval_posture: Some(next),
                }],
            ) {
                tracing::error!(session = %id, "could not reconcile Approval Posture: {error:#}");
            }
        }
    }
}
