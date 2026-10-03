//! How the Sessions this store holds reach storage: each keeps beside its
//! history what it owes storage, and a save is taken from there, encoded from
//! that one copy of the history while the store's lock is held.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};

use crate::{
    protocol::SessionId,
    storage::{HeldSessions, TakenSaves},
};

use super::SessionStoreState;

/// The store as the storage writer reads it at its idle ticks and as it
/// stops.
pub(super) struct HeldStore(pub(super) Arc<Mutex<SessionStoreState>>);

impl HeldSessions for HeldStore {
    fn take_saves(&self, quiet: Option<Duration>, whole: &[SessionId]) -> Option<TakenSaves> {
        // A caller holding the lock may be waiting on the writer, so the
        // writer never waits for it: it tries again at its next tick.
        let mut state = match self.0.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(_)) => {
                tracing::error!("the Session store failed, and nothing more of it is saved");
                return Some(TakenSaves::default());
            }
        };
        // A save waits until nothing has moved for a moment, so a Message
        // streaming in is saved once it rests rather than at every chunk.
        if let Some(quiet) = quiet
            && state
                .sessions
                .values()
                .filter_map(|record| record.unsaved.moved_at())
                .any(|moved_at| moved_at.elapsed() < quiet)
        {
            return None;
        }
        for session_id in whole {
            if let Some(record) = state.sessions.get_mut(session_id) {
                record.unsaved.rewrite_whole();
            }
        }
        Some(state.take_saves(None))
    }
}

impl SessionStoreState {
    /// Takes the save each Session owes storage — of those in `only`, where
    /// it names any — with a Sidekick's Session ahead of any whose save
    /// carries an act naming it, so the act finds it stored. A Session that
    /// cannot be encoded keeps owing its save, and the take says why.
    pub(super) fn take_saves(&mut self, only: Option<&HashSet<SessionId>>) -> TakenSaves {
        let owing = self
            .sessions
            .iter()
            .filter(|(session_id, record)| {
                record.unsaved.owes_save() && only.is_none_or(|only| only.contains(session_id))
            })
            .map(|(session_id, _)| *session_id)
            .collect::<HashSet<_>>();
        let mut taken = TakenSaves::default();
        if owing.is_empty() {
            return taken;
        }
        let sidekicks = owing
            .iter()
            .flat_map(|session_id| self.sessions[session_id].unsaved.sidekicks())
            .filter(|sidekick| owing.contains(sidekick))
            .collect::<Vec<_>>();
        let mut placed = HashSet::with_capacity(owing.len());
        for session_id in sidekicks.into_iter().chain(owing) {
            if !placed.insert(session_id) {
                continue;
            }
            let Some(record) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            match record.take_save() {
                Ok(save) => taken.saves.extend(save),
                Err(error) => {
                    tracing::error!(
                        %session_id,
                        "the Session could not be encoded for storage: {error}"
                    );
                    taken.unencoded.push(error);
                }
            }
        }
        taken
    }

    /// Marks the Session `session_id` used just now.
    pub(super) fn mark_used(&mut self, session_id: SessionId) {
        if let Some(record) = self.sessions.get_mut(&session_id) {
            record.used_at = Instant::now();
        }
    }
}
