//! Session Approval Posture overrides and live Server Setting reconciliation.

use crate::protocol::{
    ApprovalPosture, ApprovalPostureApplication, EffectiveSettings, SessionApprovalPosture,
    SessionChange, SessionId, UpdateApprovalPostureRequest,
};

use super::{SessionStore, SessionStoreState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPostureMutationError {
    SessionNotFound,
    ProviderUnavailable,
    ProviderConflict,
    InheritedByParent,
    Storage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ApprovalPostureUpdate {
    pub(crate) session_id: SessionId,
    pub(crate) value: ApprovalPosture,
    pub(crate) generation: u64,
}

pub(crate) struct ApprovalPostureMutation {
    pub(crate) posture: SessionApprovalPosture,
    pub(crate) update: Option<ApprovalPostureUpdate>,
}

impl SessionStore {
    pub(crate) fn apply_approval_posture_command(
        &self,
        session_id: SessionId,
        request: UpdateApprovalPostureRequest,
        settings: &EffectiveSettings,
    ) -> Result<ApprovalPostureMutation, ApprovalPostureMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let session = &state
            .sessions
            .get(&session_id)
            .ok_or(ApprovalPostureMutationError::SessionNotFound)?
            .snapshot
            .session;
        if session.is_subagent() {
            return Err(ApprovalPostureMutationError::InheritedByParent);
        }
        let selection = session
            .agent_selection
            .clone()
            .ok_or(ApprovalPostureMutationError::ProviderUnavailable)?;
        let (desired, pinned) = match request.posture {
            Some(posture) => {
                if posture.provider() != selection.provider {
                    return Err(ApprovalPostureMutationError::ProviderConflict);
                }
                (posture, true)
            }
            None => (
                ApprovalPosture::for_provider(&selection.provider, settings)
                    .ok_or(ApprovalPostureMutationError::ProviderUnavailable)?,
                false,
            ),
        };
        let (application, begin_generation) =
            application_for_desired(session.approval_posture.as_ref(), desired, true);
        let value = SessionApprovalPosture {
            value: desired,
            pinned,
            application,
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
        reconcile_child_approval_postures(&mut state, &self.storage)
            .map_err(|_| ApprovalPostureMutationError::Storage)?;
        let update = if application == ApprovalPostureApplication::Applying {
            let generation = if begin_generation {
                next_posture_generation(&mut state, session_id)
            } else {
                *state.posture_generations.entry(session_id).or_default()
            };
            Some(ApprovalPostureUpdate {
                session_id,
                value: desired,
                generation,
            })
        } else {
            None
        };
        Ok(ApprovalPostureMutation {
            posture: value,
            update,
        })
    }

    /// Refreshes every loaded, unpinned Session and returns all roots whose
    /// native application remains outstanding. Returning the durable Applying
    /// set, rather than only values changed by this caller, means an ordinary
    /// reader cannot consume work owed by the Settings mutation that follows.
    pub(crate) fn reconcile_approval_postures(
        &self,
        settings: &EffectiveSettings,
    ) -> Vec<ApprovalPostureUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let ids = state.sessions.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let next = state.sessions.get(&id).and_then(|record| {
                if record.snapshot.session.is_subagent()
                    || record
                        .snapshot
                        .session
                        .approval_posture
                        .as_ref()
                        .is_some_and(|posture| posture.pinned)
                {
                    return None;
                }
                let provider = &record.snapshot.session.agent_selection.as_ref()?.provider;
                let value = ApprovalPosture::for_provider(provider, settings)?;
                let (application, _) = application_for_desired(
                    record.snapshot.session.approval_posture.as_ref(),
                    value,
                    false,
                );
                Some(SessionApprovalPosture {
                    value,
                    pinned: false,
                    application,
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
                    approval_posture: Some(next.clone()),
                }],
            ) {
                tracing::error!(session = %id, "could not reconcile Approval Posture: {error:#}");
            } else if next.application == ApprovalPostureApplication::Applying {
                next_posture_generation(&mut state, id);
            }
        }
        if let Err(error) = reconcile_child_approval_postures(&mut state, &self.storage) {
            tracing::error!("could not reconcile inherited Subagent Approval Posture: {error:#}");
        }
        pending_posture_updates(&mut state)
    }

    pub(crate) fn mark_approval_posture_application(
        &self,
        update: ApprovalPostureUpdate,
        application: ApprovalPostureApplication,
    ) -> bool {
        let session_id = update.session_id;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(current) = state
            .sessions
            .get(&session_id)
            .and_then(|record| record.snapshot.session.approval_posture.clone())
        else {
            return false;
        };
        if current.value != update.value
            || state
                .posture_generations
                .get(&session_id)
                .copied()
                .unwrap_or_default()
                != update.generation
            || current.application == application
        {
            return false;
        }
        let next = SessionApprovalPosture {
            application,
            ..current
        };
        if let Err(error) = state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::ApprovalPostureChanged {
                approval_posture: Some(next),
            }],
        ) {
            tracing::error!(session = %session_id, "could not record Approval Posture application: {error:#}");
            return false;
        }
        if let Err(error) = reconcile_child_approval_postures(&mut state, &self.storage) {
            tracing::error!("could not reconcile inherited Subagent Approval Posture: {error:#}");
        }
        true
    }

    pub(crate) fn current_approval_posture_update(
        &self,
        session_id: SessionId,
    ) -> Option<ApprovalPostureUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let value = state
            .sessions
            .get(&session_id)?
            .snapshot
            .session
            .approval_posture
            .as_ref()?
            .value;
        let generation = *state.posture_generations.entry(session_id).or_default();
        Some(ApprovalPostureUpdate {
            session_id,
            value,
            generation,
        })
    }

    pub(crate) fn approval_posture_update_is_pending(&self, update: ApprovalPostureUpdate) -> bool {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state
            .sessions
            .get(&update.session_id)
            .and_then(|record| record.snapshot.session.approval_posture.as_ref())
            .is_some_and(|posture| {
                posture.value == update.value
                    && posture.application == ApprovalPostureApplication::Applying
                    && state
                        .posture_generations
                        .get(&update.session_id)
                        .copied()
                        .unwrap_or_default()
                        == update.generation
            })
    }
}

fn application_for_desired(
    current: Option<&SessionApprovalPosture>,
    desired: ApprovalPosture,
    retry_failed: bool,
) -> (ApprovalPostureApplication, bool) {
    match current {
        Some(current) if current.value == desired => match current.application {
            ApprovalPostureApplication::Failed if retry_failed => {
                (ApprovalPostureApplication::Applying, true)
            }
            application => (application, false),
        },
        _ => (ApprovalPostureApplication::Applying, true),
    }
}

fn next_posture_generation(state: &mut SessionStoreState, session_id: SessionId) -> u64 {
    let generation = state.posture_generations.entry(session_id).or_default();
    *generation = generation.wrapping_add(1);
    *generation
}

fn pending_posture_updates(state: &mut SessionStoreState) -> Vec<ApprovalPostureUpdate> {
    let pending = state
        .sessions
        .iter()
        .filter_map(|(session_id, record)| {
            let posture = record.snapshot.session.approval_posture.as_ref()?;
            (!record.snapshot.session.is_subagent()
                && posture.application == ApprovalPostureApplication::Applying)
                .then_some((*session_id, posture.value))
        })
        .collect::<Vec<_>>();
    pending
        .into_iter()
        .map(|(session_id, value)| ApprovalPostureUpdate {
            session_id,
            value,
            generation: *state.posture_generations.entry(session_id).or_default(),
        })
        .collect()
}

/// Child Sessions share their root Session's native Provider actor, so their
/// posture is an inherited reading rather than an independently mutable value.
fn reconcile_child_approval_postures(
    state: &mut SessionStoreState,
    storage: &crate::storage::StorageSink,
) -> anyhow::Result<()> {
    let children = state
        .sessions
        .iter()
        .filter_map(|(id, record)| record.snapshot.session.is_subagent().then_some(*id))
        .collect::<Vec<_>>();
    for child in children {
        let mut root = child;
        let mut visited = std::collections::HashSet::new();
        let inherited = loop {
            if !visited.insert(root) {
                break None;
            }
            let Some(record) = state.sessions.get(&root) else {
                break None;
            };
            match record.snapshot.session.parent {
                Some(parent) => root = parent,
                None => break Some(record.snapshot.session.approval_posture),
            }
        };
        // Restoration deliberately retains readable cyclic components and children whose parent
        // is missing, but does not promote either into a root. They have no authoritative root
        // posture to inherit, so leave their persisted reading untouched.
        let Some(inherited) = inherited else {
            continue;
        };
        if state.sessions[&child].snapshot.session.approval_posture == inherited {
            continue;
        }
        state.commit(
            storage,
            child,
            vec![SessionChange::ApprovalPostureChanged {
                approval_posture: inherited,
            }],
        )?;
    }
    Ok(())
}
