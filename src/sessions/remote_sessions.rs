//! The Sessions a Sidekick has a hand in on a Remote, as that Remote says of
//! them now.
//!
//! Only the Sidekick's own Server knows it acted on a Remote's Session, so
//! the act is recorded here (see [`super::sidekick_acts`]); what the Session
//! is doing is the Remote's to say. While some Client watches a tree listing
//! a Remote's Session, the Server keeps that Remote in view — reading what it
//! holds and then following each change to it, as a Client keeping a Remote
//! in view under Everywhere does — and holds here what it last said of the
//! Sessions acted on there, which the tree draws them from. A Remote that does
//! not answer is held as silent, and its Sessions stand in the tree with
//! nothing of what it said before, so nothing stale is given as current; one
//! not yet read stands in no tree until it is. What a Remote says is held
//! only while it is kept in view, and forgotten once no tree lists it.
//!
//! A Remote's Session found deleted — absent from what the Remote holds when
//! it is read, or deleted while it is followed — is dropped: every act on it
//! is forgotten, here and in storage, and it leaves every tree listing it.

use std::collections::{HashMap, HashSet};

use crate::protocol::{
    ActivityStatus, Outlook, SessionCatalogChange, SessionId, SessionListItem, SessionReference,
    SessionSummary, SessionTimestamp, SubagentTreeSession, TurnStatus,
};

use super::{SessionStore, SessionStoreState};

/// What each Remote kept in view last said of the Sessions acted on there, by
/// the Remote's name.
#[derive(Default)]
pub(super) struct RemoteReadings {
    by_remote: HashMap<String, RemoteReading>,
}

/// What one Remote kept in view last said.
enum RemoteReading {
    /// It answered: each Session acted on there that it holds, as it holds
    /// it now.
    Answering(HashMap<SessionId, SessionSummary>),
    /// It does not answer now.
    Silent,
}

impl SessionStore {
    /// The Remotes holding a Session that the Sidekick heading the tree
    /// `session_id` belongs to acted on, which that tree lists: none for a
    /// tree no Sidekick's Session heads.
    pub(crate) fn remotes_listed_with(&self, session_id: SessionId) -> Vec<String> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(top_level) = state.top_level_of(session_id) else {
            return Vec::new();
        };
        let head = state.tree_head(top_level);
        if !state.is_sidekicks(head) {
            return Vec::new();
        }
        let mut remotes = state
            .sidekick_acts
            .everywhere_of(head)
            .filter_map(|(acted_on, _)| acted_on.origin.remote_name().map(str::to_owned))
            .collect::<Vec<_>>();
        remotes.sort();
        remotes.dedup();
        remotes
    }

    /// Whether some Client watches a tree listing a Session of the Remote
    /// `remote`, so it is still to be kept in view.
    pub(crate) fn is_remote_watched(&self, remote: &str) -> bool {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state
            .sidekicks_acting_on(remote)
            .into_iter()
            .any(|sidekick| state.subagent_trees.is_watched(sidekick))
    }

    /// Takes up what the Remote `remote` holds, `listed` as its own listing
    /// of its top-level Sessions gives them: each Session acted on there
    /// stands as listed, and one it no longer holds is dropped.
    pub(crate) fn remote_read(&self, remote: &str, listed: Vec<SessionListItem>) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let acted_on = state.sessions_acted_on_at(remote);
        let mut held = HashMap::new();
        let mut present = HashSet::new();
        for item in listed {
            present.insert(item.id());
            if let SessionListItem::Readable(summary) = item
                && acted_on.contains(&summary.session.id)
            {
                held.insert(summary.session.id, *summary);
            }
        }
        state
            .remote_readings
            .by_remote
            .insert(remote.to_owned(), RemoteReading::Answering(held));
        for gone in acted_on.difference(&present) {
            self.drop_remote_session(&mut state, remote, *gone);
        }
        state.announce_trees_listing(remote);
    }

    /// Takes up one change the Remote `remote` said of what it holds, as its
    /// own catalog said it.
    pub(crate) fn remote_changed(&self, remote: &str, change: SessionCatalogChange) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let SessionCatalogChange::Deleted { session_id } = change {
            self.drop_remote_session(&mut state, remote, session_id);
            state.announce_trees_listing(remote);
            return;
        }
        let Some(RemoteReading::Answering(held)) = state.remote_readings.by_remote.get_mut(remote)
        else {
            return;
        };
        let moved = match change {
            SessionCatalogChange::TitleChanged {
                session_id,
                title,
                icon,
            } => held.get_mut(&session_id).map(|summary| {
                summary.title = title;
                summary.icon = icon;
            }),
            SessionCatalogChange::SettlementChanged {
                session_id,
                settled_at,
            } => held
                .get_mut(&session_id)
                .map(|summary| summary.settled_at = settled_at),
            SessionCatalogChange::WorkingChanged {
                session_id,
                working_since,
            } => held
                .get_mut(&session_id)
                .map(|summary| summary.session.working_since = working_since),
            SessionCatalogChange::MonitoringChanged {
                session_id,
                monitoring_since,
            } => held
                .get_mut(&session_id)
                .map(|summary| summary.session.monitoring_since = monitoring_since),
            SessionCatalogChange::StandingInputsChanged { session_id, inputs } => held
                .get_mut(&session_id)
                .map(|summary| summary.standing_inputs = inputs),
            SessionCatalogChange::WorkspaceIconChanged { workspace_id, icon } => {
                for summary in held.values_mut() {
                    if summary.session.workspace.id == workspace_id {
                        summary.session.workspace.icon.clone_from(&icon);
                    }
                }
                Some(())
            }
            SessionCatalogChange::Invalidated { .. }
            | SessionCatalogChange::CheckoutStateChanged { .. }
            | SessionCatalogChange::Created { .. }
            | SessionCatalogChange::Deleted { .. }
            | SessionCatalogChange::UsageChanged { .. }
            | SessionCatalogChange::WorkspaceDescriptionChanged { .. } => None,
        };
        if moved.is_some() {
            state.announce_trees_listing(remote);
        }
    }

    /// Takes up that the Remote `remote` does not answer now: its Sessions
    /// stand in every tree listing them with nothing of what it said before.
    pub(crate) fn remote_silent(&self, remote: &str) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state
            .remote_readings
            .by_remote
            .insert(remote.to_owned(), RemoteReading::Silent);
        state.announce_trees_listing(remote);
    }

    /// Takes up that the Pairing with the Remote `remote` has ended: its
    /// Sessions leave every tree listing them, as a Remote whose Pairing has
    /// ended takes its rows with it, though the acts on them are kept.
    pub(crate) fn remote_unpaired(&self, remote: &str) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state.remote_readings.by_remote.remove(remote);
        state.announce_trees_listing(remote);
    }

    /// Forgets what the Remote `remote` said, now it is kept in view no
    /// longer, so it is read afresh when it is again.
    pub(crate) fn remote_unwatched(&self, remote: &str) {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .remote_readings
            .by_remote
            .remove(remote);
    }

    /// Forgets every act on the Session `session_id` of the Remote `remote`,
    /// found deleted there, here and in storage.
    fn drop_remote_session(
        &self,
        state: &mut SessionStoreState,
        remote: &str,
        session_id: SessionId,
    ) {
        let acted_on = SessionReference::new(Outlook::Remote(remote.to_owned()), session_id);
        if state.sidekick_acts.forget_on(&acted_on).is_empty() {
            return;
        }
        if let Some(RemoteReading::Answering(held)) =
            state.remote_readings.by_remote.get_mut(remote)
        {
            held.remove(&session_id);
        }
        self.storage
            .forget_remote_sidekick_acts(remote.to_owned(), session_id);
    }
}

impl SessionStoreState {
    /// The Sidekicks' Sessions that acted on a Session of the Remote
    /// `remote`.
    fn sidekicks_acting_on(&self, remote: &str) -> HashSet<SessionId> {
        self.sidekick_acts
            .all()
            .filter(|(_, acted_on)| acted_on.origin.remote_name() == Some(remote))
            .map(|(sidekick, _)| sidekick)
            .collect()
    }

    /// The Sessions of the Remote `remote` some Sidekick acted on.
    fn sessions_acted_on_at(&self, remote: &str) -> HashSet<SessionId> {
        self.sidekick_acts
            .all()
            .filter(|(_, acted_on)| acted_on.origin.remote_name() == Some(remote))
            .map(|(_, acted_on)| acted_on.session_id)
            .collect()
    }

    /// Tells the subscribers of every tree listing a Session of the Remote
    /// `remote` what moved in it.
    fn announce_trees_listing(&mut self, remote: &str) {
        for sidekick in self.sidekicks_acting_on(remote) {
            self.announce_tree_headed_by(sidekick);
        }
    }

    /// The entries for the Sessions the Sidekick's Session `sidekick` acted
    /// on at Remotes, with the moment of its latest act on each: as each
    /// Remote last said of them, with nothing of what it said where it does
    /// not answer now, and none for a Remote not read since it was kept in
    /// view, or a Session it was not found to hold.
    pub(super) fn remote_sessions_beneath(&self, sidekick: SessionId) -> Vec<SubagentTreeSession> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        self.sidekick_acts
            .everywhere_of(sidekick)
            .filter_map(|(acted_on, acted_at)| {
                let remote = acted_on.origin.remote_name()?;
                match self.remote_readings.by_remote.get(remote)? {
                    RemoteReading::Answering(held) => Some(remote_session(
                        remote,
                        held.get(&acted_on.session_id)?,
                        acted_at,
                    )),
                    RemoteReading::Silent => Some(SubagentTreeSession {
                        session_id: acted_on.session_id,
                        origin: Some(remote.to_owned()),
                        unanswered: true,
                        title: String::new(),
                        subsession: false,
                        workspace_path: Default::default(),
                        workspace_icon: None,
                        model: None,
                        status: None,
                        worked_ms: None,
                        working_since: None,
                        monitoring_since: None,
                        needs_intervention: false,
                        acted_at,
                    }),
                }
            })
            .collect()
    }
}

/// The entry for the Session of the Remote `remote` that `summary` lists, as
/// its listing there says it, with the moment of the Sidekick's latest act on
/// it. It stands as a Session the Sidekick only acted on, whether or not the
/// Sidekick began it there, since opening it shows the tree it heads on that
/// Remote. Its listing says when it works and how its latest Turn settled,
/// and nothing of how long its settled Turns took, so its time is left unsaid
/// once it settles.
fn remote_session(
    remote: &str,
    summary: &SessionSummary,
    acted_at: SessionTimestamp,
) -> SubagentTreeSession {
    let session = &summary.session;
    let status = if session.working_since.is_some() {
        Some(ActivityStatus::Active)
    } else {
        summary
            .standing_inputs
            .latest_turn
            .as_ref()
            .map(|latest| match latest.status {
                TurnStatus::Active => ActivityStatus::Active,
                TurnStatus::Completed => ActivityStatus::Completed,
                TurnStatus::Failed => ActivityStatus::Failed,
                TurnStatus::Interrupted => ActivityStatus::Interrupted,
            })
    };
    SubagentTreeSession {
        session_id: session.id,
        origin: Some(remote.to_owned()),
        unanswered: false,
        title: summary.title.clone(),
        subsession: false,
        workspace_path: session.workspace.path.clone(),
        workspace_icon: session.workspace.icon.clone(),
        model: session
            .agent_selection
            .as_ref()
            .map(|selection| selection.model.clone()),
        status,
        worked_ms: session.working_since.map(|_| 0),
        working_since: session.working_since,
        monitoring_since: session.monitoring_since,
        needs_intervention: !summary.standing_inputs.pending_questionnaires.is_empty()
            || !summary.standing_inputs.pending_approvals.is_empty(),
        acted_at,
    }
}
