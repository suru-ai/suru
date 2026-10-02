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
//! A Remote's Session found deleted is dropped — every act on it forgotten,
//! here and in storage, and it leaves every tree listing it — and only where
//! that is confirmed: the Remote says so, reading the Session finds it gone,
//! or the Remote's listing of its top-level Sessions, asked for after the
//! act was recorded, holds it no longer where it is known to be one. An act
//! on a Subagent's Session there stands by the Session heading it, which the
//! Remote is asked for; one it has not yet said is kept, unresolved, standing
//! in no tree and taken as deleted by no listing, and asked again whenever
//! the Remote is next read.

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
    /// It does not answer now: each Session acted on there as it last said
    /// of it, where it said anything since it was kept in view — whose Title
    /// and Workspace still say which Session it is, and nothing else of it
    /// is current.
    Silent(HashMap<SessionId, SessionSummary>),
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

    /// A moment now on the store's clock, which every act recorded after it
    /// is later than: what a listing of a Remote is asked for at, so only an
    /// act recorded before it is judged by it.
    pub(crate) fn moment(&self) -> SessionTimestamp {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .next_timestamp()
    }

    /// Takes up what the Remote `remote` holds, `listed` as its own listing
    /// of its top-level Sessions gives them, asked for at `asked_at`: each
    /// Session acted on there stands as listed, and one it no longer holds
    /// is dropped — where every act on it was recorded before the listing
    /// was asked for, and it is known to head its own tree there.
    pub(crate) fn remote_read(
        &self,
        remote: &str,
        listed: Vec<SessionListItem>,
        asked_at: SessionTimestamp,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let acted_on = state.sessions_acted_on_at(remote);
        let present = listed
            .iter()
            .map(SessionListItem::id)
            .collect::<HashSet<_>>();
        let held = listed
            .into_iter()
            .filter_map(|item| match item {
                SessionListItem::Readable(summary) if acted_on.contains(&summary.session.id) => {
                    Some((summary.session.id, *summary))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        for (session_id, summary) in &held {
            self.follow_title(&mut state, remote, *session_id, &summary.title);
        }
        state
            .remote_readings
            .by_remote
            .insert(remote.to_owned(), RemoteReading::Answering(held));
        self.drop_unlisted(&mut state, remote, &present, asked_at);
        state.announce_trees_listing(remote);
    }

    /// Has the row leading into the Session `session_id` of the Remote
    /// `remote`, in the Transcript of each Sidekick that began it there, name
    /// `title`, the Title that Remote gives it now.
    fn follow_title(
        &self,
        state: &mut SessionStoreState,
        remote: &str,
        session_id: SessionId,
        title: &str,
    ) {
        let began = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, acted_on, act)| *acted_on == session_id && act.began)
            .map(|(sidekick, ..)| sidekick)
            .collect::<Vec<_>>();
        if !began.is_empty() {
            state.follow_remote_subsession_title(&self.storage, &began, remote, session_id, title);
        }
    }

    /// Takes up the Remote `remote`'s listing of its top-level Sessions,
    /// `listed`, asked for at `asked_at` for a reader of it — a Sidekick's
    /// listing there, or Everywhere: a Session acted on there it no longer
    /// holds is dropped as [`Self::remote_read`] drops one.
    pub(crate) fn remote_listed(
        &self,
        remote: &str,
        listed: &[SessionListItem],
        asked_at: SessionTimestamp,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let present = listed
            .iter()
            .map(SessionListItem::id)
            .collect::<HashSet<_>>();
        self.drop_unlisted(&mut state, remote, &present, asked_at);
        state.announce_trees_listing(remote);
    }

    /// Drops each Session of the Remote `remote` acted on that is not among
    /// `present`, its listing of its top-level Sessions asked for at
    /// `asked_at`, where every act on it was recorded before then and stands
    /// by it as a Session heading its own tree.
    fn drop_unlisted(
        &self,
        state: &mut SessionStoreState,
        remote: &str,
        present: &HashSet<SessionId>,
        asked_at: SessionTimestamp,
    ) {
        let gone = gone_from(
            state
                .sidekick_acts
                .at_remote(remote)
                .map(|(_, session_id, act)| (session_id, act)),
            present,
            asked_at,
        );
        for session_id in gone {
            self.drop_remote_session(state, remote, session_id);
        }
    }

    /// The Sessions of the Remote `remote` acted on whose acts do not yet
    /// stand by the Session heading them there.
    pub(crate) fn unresolved_remote_acts(&self, remote: &str) -> Vec<SessionId> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut unresolved = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, _, act)| !act.resolved)
            .map(|(_, session_id, _)| session_id)
            .collect::<Vec<_>>();
        unresolved.sort_by_key(|session_id| session_id.as_uuid());
        unresolved.dedup();
        unresolved
    }

    /// Stands every act on the Session `session_id` of the Remote `remote`,
    /// not yet resolved, by `top_level`, the Session the Remote says heads
    /// it — itself, where it is no Subagent's.
    pub(crate) fn resolve_remote_acts(
        &self,
        remote: &str,
        session_id: SessionId,
        top_level: SessionId,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let unresolved = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, acted_on, act)| *acted_on == session_id && !act.resolved)
            .collect::<Vec<_>>();
        if unresolved.is_empty() {
            return;
        }
        if top_level != session_id {
            state.sidekick_acts.forget_on(&SessionReference::new(
                Outlook::Remote(remote.to_owned()),
                session_id,
            ));
            self.storage
                .forget_remote_sidekick_acts(remote.to_owned(), session_id);
        }
        for (sidekick, _, act) in unresolved {
            self.land_remote_act(
                &mut state,
                sidekick,
                remote,
                top_level,
                super::sidekick_acts::Act {
                    resolved: true,
                    ..act
                },
            );
        }
    }

    /// Forgets every act on the Session `session_id` of the Remote `remote`,
    /// which a read of it, or an act on it, found the Remote no longer
    /// holds.
    pub(crate) fn forget_remote_session(&self, remote: &str, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        self.drop_remote_session(&mut state, remote, session_id);
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
            } => {
                let moved = held.get_mut(&session_id).map(|summary| {
                    summary.title.clone_from(&title);
                    summary.icon = icon;
                });
                self.follow_title(&mut state, remote, session_id, &title);
                moved
            }
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
            | SessionCatalogChange::WorkspaceDescriptionChanged { .. }
            | SessionCatalogChange::RemoteSubsessionsChanged { .. } => None,
        };
        if moved.is_some() {
            state.announce_trees_listing(remote);
        }
    }

    /// Takes up that the Remote `remote` does not answer now: its Sessions
    /// stand in every tree listing them by what last said which each is —
    /// its Title and Workspace — and as not answering, nothing of their work
    /// given as current.
    pub(crate) fn remote_silent(&self, remote: &str) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let last = match state.remote_readings.by_remote.remove(remote) {
            Some(RemoteReading::Answering(held) | RemoteReading::Silent(held)) => held,
            None => HashMap::new(),
        };
        state
            .remote_readings
            .by_remote
            .insert(remote.to_owned(), RemoteReading::Silent(last));
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
        let sidekicks = state.sidekick_acts.forget_on(&acted_on);
        if sidekicks.is_empty() {
            return;
        }
        if let Some(RemoteReading::Answering(held) | RemoteReading::Silent(held)) =
            state.remote_readings.by_remote.get_mut(remote)
        {
            held.remove(&session_id);
        }
        // A Sidekick that acted on nothing else there is no longer among
        // those listing the Remote, so each is told here.
        for sidekick in sidekicks {
            state.announce_tree_headed_by(sidekick);
            if let Some(change) = state.note_remote_subsessions(sidekick) {
                state.publish_catalog_change(change);
            }
        }
        self.storage
            .forget_remote_sidekick_acts(remote.to_owned(), session_id);
    }
}

impl SessionStoreState {
    /// The Sessions of the Remote `remote` some Sidekick acted on.
    fn sessions_acted_on_at(&self, remote: &str) -> HashSet<SessionId> {
        self.sidekick_acts
            .at_remote(remote)
            .map(|(_, session_id, _)| session_id)
            .collect()
    }

    /// The Sidekicks' Sessions that acted on a Session of the Remote
    /// `remote`.
    fn sidekicks_acting_on(&self, remote: &str) -> HashSet<SessionId> {
        self.sidekick_acts
            .all()
            .filter(|(_, acted_on)| acted_on.origin.remote_name() == Some(remote))
            .map(|(sidekick, _)| sidekick)
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
            .filter(|(_, act)| act.resolved)
            .filter_map(|(acted_on, act)| {
                let acted_at = act.acted_at;
                let remote = acted_on.origin.remote_name()?;
                match self.remote_readings.by_remote.get(remote)? {
                    RemoteReading::Answering(held) => Some(remote_session(
                        remote,
                        held.get(&acted_on.session_id)?,
                        acted_at,
                    )),
                    RemoteReading::Silent(last) => {
                        let last = last.get(&acted_on.session_id);
                        Some(SubagentTreeSession {
                            session_id: acted_on.session_id,
                            origin: Some(remote.to_owned()),
                            unanswered: true,
                            title: last
                                .map(|summary| summary.title.clone())
                                .unwrap_or_default(),
                            subsession: false,
                            workspace_path: last
                                .map(|summary| summary.session.workspace.path.clone())
                                .unwrap_or_default(),
                            workspace_icon: last
                                .and_then(|summary| summary.session.workspace.icon.clone()),
                            model: None,
                            status: None,
                            worked_ms: None,
                            working_since: None,
                            monitoring_since: None,
                            needs_intervention: false,
                            acted_at,
                        })
                    }
                }
            })
            .collect()
    }
}

/// The Sessions acted on, each act among `acts`, that a Remote's listing of
/// its top-level Sessions asked for at `asked_at`, holding `present`,
/// confirms it no longer holds: one missing from it whose every act was
/// recorded before the listing was asked for — so the listing can have
/// answered of it — and stands by a Session known to head its own tree, as a
/// Subagent's never shows in that listing.
fn gone_from(
    acts: impl Iterator<Item = (SessionId, super::sidekick_acts::Act)>,
    present: &HashSet<SessionId>,
    asked_at: SessionTimestamp,
) -> Vec<SessionId> {
    let mut judged = HashMap::<SessionId, bool>::new();
    for (session_id, act) in acts {
        *judged.entry(session_id).or_insert(true) &= act.resolved && act.acted_at < asked_at;
    }
    let mut gone = judged
        .into_iter()
        .filter(|(session_id, judged)| *judged && !present.contains(session_id))
        .map(|(session_id, _)| session_id)
        .collect::<Vec<_>>();
    gone.sort_by_key(|session_id| session_id.as_uuid());
    gone
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::sidekick_acts::Act;

    fn act(acted_at: u64, resolved: bool) -> Act {
        Act {
            acted_at: SessionTimestamp(acted_at),
            began: false,
            resolved,
        }
    }

    #[test]
    fn only_what_a_listing_could_have_answered_of_is_taken_as_gone_from_it() {
        let (deleted, begun_meanwhile, acted_meanwhile, listed, subagents) = (
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
        );
        let present = HashSet::from([listed]);
        assert_eq!(
            gone_from(
                [
                    (deleted, act(5, true)),
                    (begun_meanwhile, act(11, true)),
                    (acted_meanwhile, act(4, true)),
                    (acted_meanwhile, act(12, true)),
                    (listed, act(3, true)),
                    (subagents, act(2, false)),
                ]
                .into_iter(),
                &present,
                SessionTimestamp(10),
            ),
            [deleted],
            "a Session begun or acted on after the listing was asked for, one listed, and one \
             not known to head its own tree are kept"
        );
    }
}
