//! Session Approval Posture overrides and live Server Setting reconciliation.

use crate::protocol::{
    ApprovalPosture, ApprovalPostureApplication, EffectiveSettings, SessionApprovalPosture,
    SessionChange, SessionId, UpdateApprovalPostureRequest,
};

use super::{SessionStore, SessionStoreState};
use crate::storage::StorageSink;

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
                        approval_posture: Some(value),
                    }],
                )
                .map_err(|_| ApprovalPostureMutationError::Storage)?;
        }
        if !state.inherit_tree_postures(&self.storage, [session_id]) {
            return Err(ApprovalPostureMutationError::Storage);
        }
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

    /// Refreshes every hydrated, unpinned Session against `settings` and
    /// returns all roots whose native application remains outstanding.
    /// Returning the durable Applying set, rather than only values changed by
    /// this caller, means an ordinary reader cannot consume work owed by the
    /// Settings mutation that follows.
    ///
    /// This is the Settings adoption refresh: a Settings change is the one
    /// event that can leave a loaded posture behind. Deferred Sessions are
    /// passed over, since hydration is what brings each of them up to date.
    pub(crate) fn reconcile_approval_postures(
        &self,
        settings: &EffectiveSettings,
    ) -> Vec<ApprovalPostureUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let roots = state
            .sessions
            .iter()
            .filter(|(id, record)| {
                !record.snapshot.session.is_subagent() && !state.is_deferred(**id)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for &root in &roots {
            state.follow_settings(&self.storage, root, settings, PostureDelivery::Owed);
        }
        state.inherit_tree_postures(&self.storage, roots);
        pending_posture_updates(&mut state)
    }

    /// Refreshes the tree `session_id` belongs to — its root's unpinned
    /// posture and its Subagents' inherited reading — and returns the root's
    /// outstanding native application, if any. This is what a change confined
    /// to one Session (its creation, its Agent Selection, its Provider
    /// starting) reconciles, without walking every other Session the store
    /// holds.
    pub(crate) fn reconcile_tree_approval_posture(
        &self,
        session_id: SessionId,
        settings: &EffectiveSettings,
    ) -> Option<ApprovalPostureUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        let root = state.posture_root(session_id)?;
        state.follow_settings(&self.storage, root, settings, PostureDelivery::Owed);
        state.inherit_tree_postures(&self.storage, [root]);
        state.outstanding_posture_update(root)
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
            .and_then(|record| record.snapshot.session.approval_posture)
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
            tracing::warn!(session = %session_id, "could not record Approval Posture application: {error:#}");
            return false;
        }
        state.inherit_tree_postures(&self.storage, [session_id]);
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
    let roots = state
        .sessions
        .iter()
        .filter(|(_, record)| !record.snapshot.session.is_subagent())
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .filter_map(|root| state.outstanding_posture_update(root))
        .collect()
}

/// Whether a posture that changes here has a live Provider to reach.
#[derive(Clone, Copy)]
enum PostureDelivery {
    /// The Session's Provider is owed the value, and a reader may consume
    /// that debt as an update to deliver.
    Owed,
    /// No Provider exists to owe it to, so the value is recorded as applied
    /// at once: whatever Provider starts next starts under it.
    Settled,
}

impl SessionStoreState {
    /// Brings every Session in `hydrated` up to `settings`. A Session
    /// hydrated just now had no Provider actor, since every path that starts
    /// one hydrates first, so nothing is owed to a live Provider: a value
    /// that changes here, or one still reading Applying from the process this
    /// history outlived, is recorded as applied.
    pub(super) fn adopt_hydrated_postures(
        &mut self,
        storage: &StorageSink,
        hydrated: &[SessionId],
        settings: &EffectiveSettings,
    ) {
        let roots = hydrated
            .iter()
            .filter_map(|&id| self.posture_root(id))
            .collect::<std::collections::HashSet<_>>();
        for &root in &roots {
            self.follow_settings(storage, root, settings, PostureDelivery::Settled);
        }
        self.inherit_tree_postures(storage, roots);
    }

    /// Sets `root`'s unpinned posture to what `settings` say for its
    /// Provider. A value that does not change keeps whatever application it
    /// had reached, except that a Settled delivery closes an Applying one.
    fn follow_settings(
        &mut self,
        storage: &StorageSink,
        root: SessionId,
        settings: &EffectiveSettings,
        delivery: PostureDelivery,
    ) {
        let Some(next) = self.sessions.get(&root).and_then(|record| {
            let session = &record.snapshot.session;
            if session.is_subagent()
                || session
                    .approval_posture
                    .as_ref()
                    .is_some_and(|posture| posture.pinned)
            {
                return None;
            }
            let provider = &session.agent_selection.as_ref()?.provider;
            let value = ApprovalPosture::for_provider(provider, settings)?;
            let (application, _) =
                application_for_desired(session.approval_posture.as_ref(), value, false);
            let application = match delivery {
                PostureDelivery::Owed => application,
                PostureDelivery::Settled if application == ApprovalPostureApplication::Applying => {
                    ApprovalPostureApplication::Applied
                }
                PostureDelivery::Settled => application,
            };
            Some(SessionApprovalPosture {
                value,
                pinned: false,
                application,
            })
        }) else {
            return;
        };
        if self.sessions[&root]
            .snapshot
            .session
            .approval_posture
            .as_ref()
            == Some(&next)
        {
            return;
        }
        let applying = next.application == ApprovalPostureApplication::Applying;
        if self.commit_posture(storage, root, Some(next)) && applying {
            next_posture_generation(self, root);
        }
    }

    /// Child Sessions share their root Session's native Provider actor, so
    /// their posture is an inherited reading rather than an independently
    /// mutable value. Every hydrated descendant of a root in `roots` takes
    /// its reading in one pass over the store; deferred descendants take it
    /// when they are hydrated. Reports whether every reading landed.
    fn inherit_tree_postures(
        &mut self,
        storage: &StorageSink,
        roots: impl IntoIterator<Item = SessionId>,
    ) -> bool {
        let roots = roots.into_iter().collect::<std::collections::HashSet<_>>();
        let readings = self
            .sessions
            .iter()
            .filter(|(id, record)| record.snapshot.session.is_subagent() && !self.is_deferred(**id))
            .filter_map(|(id, record)| {
                let root = self.posture_root(*id).filter(|root| roots.contains(root))?;
                let inherited = self.sessions.get(&root)?.snapshot.session.approval_posture;
                (record.snapshot.session.approval_posture != inherited).then_some((*id, inherited))
            })
            .collect::<Vec<_>>();
        let mut landed = true;
        for (child, inherited) in readings {
            landed &= self.commit_posture(storage, child, inherited);
        }
        landed
    }

    /// The root's native application still outstanding, as an update a
    /// reader may deliver to its Provider.
    fn outstanding_posture_update(&mut self, root: SessionId) -> Option<ApprovalPostureUpdate> {
        let posture = self
            .sessions
            .get(&root)?
            .snapshot
            .session
            .approval_posture?;
        (posture.application == ApprovalPostureApplication::Applying).then(|| {
            ApprovalPostureUpdate {
                session_id: root,
                value: posture.value,
                generation: *self.posture_generations.entry(root).or_default(),
            }
        })
    }

    /// The root whose posture `session_id` reads. Restoration deliberately
    /// retains readable cyclic components and children whose parent is
    /// missing, but does not promote either into a root: they have no
    /// authoritative root posture, so they resolve to none.
    fn posture_root(&self, session_id: SessionId) -> Option<SessionId> {
        let mut current = session_id;
        let mut visited = std::collections::HashSet::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            let record = self.sessions.get(&current)?;
            match record.snapshot.session.parent {
                Some(parent) => current = parent,
                None => return Some(current),
            }
        }
    }

    /// Reconciliation is best-effort: a refusal is logged and the reading it
    /// would have corrected stands until the next reconcile.
    fn commit_posture(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        approval_posture: Option<SessionApprovalPosture>,
    ) -> bool {
        match self.commit(
            storage,
            session_id,
            vec![SessionChange::ApprovalPostureChanged { approval_posture }],
        ) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(session = %session_id, "could not reconcile Approval Posture: {error:#}");
                false
            }
        }
    }
}
