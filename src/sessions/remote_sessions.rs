//! The Sessions a Sidekick has a hand in on a Remote, as that Remote says of
//! them now.
//!
//! Only the Sidekick's own Server knows it acted on a Remote's Session, so
//! the act is recorded here (see [`super::sidekick_acts`]); what the Session
//! is doing is the Remote's to say. While some Client watches a tree listing
//! a Remote's Session, the Server keeps that Remote in view — reading what it
//! holds and then following each change to it, as a Client keeping a Remote
//! in view under Everywhere does — and holds here what it last said of the
//! Sessions acted on there, and of the tree each heads there, which the tree
//! draws them and the Subagents beneath them from. A Remote that does not
//! answer is held as silent, and its Sessions stand in the tree by what last
//! said which each is, with nothing of their work given as current and no
//! Subagents beneath them; one not yet read stands in no tree until it is.
//! What a Remote says is held only while it is kept in view, and forgotten
//! once no tree lists it.
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
    SessionSummary, SessionTimestamp, SubagentTreeChange, SubagentTreeEntry, SubagentTreeSession,
    SubagentTreeSnapshot, TurnStatus,
};

use super::{SessionStore, SessionStoreState};

/// A beginning on a Remote a read there confirmed: the Sidekick's Session
/// that began it, the Session begun, the Title it is given there, and what it
/// was first asked — what the row leading into it in the Sidekick's
/// Transcript names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfirmedBeginning {
    pub(crate) sidekick: SessionId,
    pub(crate) session_id: SessionId,
    pub(crate) title: String,
    pub(crate) prompt: String,
}

/// The Sessions a listing holds, `present`, each by the Title `held` gives
/// it where the listing could be read for it.
fn listed_titles(
    held: &HashMap<SessionId, SessionSummary>,
    present: &HashSet<SessionId>,
) -> HashMap<SessionId, Option<String>> {
    present
        .iter()
        .map(|session_id| {
            (
                *session_id,
                held.get(session_id).map(|summary| summary.title.clone()),
            )
        })
        .collect()
}

/// What each Remote kept in view last said of the Sessions acted on there, by
/// the Remote's name.
#[derive(Default)]
pub(super) struct RemoteReadings {
    by_remote: HashMap<String, RemoteReading>,
    /// The tree each Session acted on at an answering Remote heads there,
    /// as that Remote last said of it, where it is followed: by the Remote's
    /// name, then by the Session's identity there.
    trees: HashMap<String, HashMap<SessionId, RemoteTree>>,
    /// The Sessions acted on at each Remote whose trees there are not
    /// followed, for want of room among the trees this Server follows.
    unfollowed: HashMap<String, HashSet<SessionId>>,
}

/// The tree a Session acted on at a Remote heads there — its own branch of
/// it, where the tree is its Sidekick's there — as that Remote last said of
/// it, kept within bounds: `cut` where some of its Subagents were left out
/// for running past them.
struct RemoteTree {
    tree: SubagentTreeSnapshot,
    cut: bool,
}

/// How much of one Remote Session's tree is kept: how many of its
/// Subagents, and how deep beneath it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TreeBounds {
    pub(crate) entries: usize,
    pub(crate) depth: usize,
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
    /// of its top-level Sessions gives them through the Pairing whose key
    /// fingerprint is `pairing`, asked for at `asked_at`: each Session acted
    /// on there stands as listed — an act on it not yet confirmed confirmed
    /// by its being there — and one it no longer holds is dropped, where
    /// every act on it was recorded before the listing was asked for and it
    /// is known to head its own tree there. Answers each beginning this
    /// confirmed, whose row is to stand in its Sidekick's Transcript.
    pub(crate) fn remote_read(
        &self,
        remote: &str,
        pairing: &str,
        listed: Vec<SessionListItem>,
        asked_at: SessionTimestamp,
    ) -> Vec<ConfirmedBeginning> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        self.keep_pairing(&mut state, remote, pairing);
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
        if let Some(trees) = state.remote_readings.trees.get_mut(remote) {
            trees.retain(|session_id, _| held.contains_key(session_id));
        }
        let confirmed = self.confirm_listed(&mut state, remote, &listed_titles(&held, &present));
        state
            .remote_readings
            .by_remote
            .insert(remote.to_owned(), RemoteReading::Answering(held));
        self.drop_unlisted(&mut state, remote, &present, asked_at);
        state.announce_trees_listing(remote);
        confirmed
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
        pairing: &str,
        listed: &[SessionListItem],
        asked_at: SessionTimestamp,
    ) -> Vec<ConfirmedBeginning> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        self.keep_pairing(&mut state, remote, pairing);
        let present = listed
            .iter()
            .map(SessionListItem::id)
            .collect::<HashSet<_>>();
        let titles = listed
            .iter()
            .filter_map(|item| match item {
                SessionListItem::Readable(summary) => {
                    Some((summary.session.id, summary.title.clone()))
                }
                SessionListItem::Unreadable(_) => None,
            })
            .collect::<HashMap<_, _>>();
        let titles = present
            .iter()
            .map(|session_id| (*session_id, titles.get(session_id).cloned()))
            .collect::<HashMap<_, _>>();
        let confirmed = self.confirm_listed(&mut state, remote, &titles);
        self.drop_unlisted(&mut state, remote, &present, asked_at);
        state.announce_trees_listing(remote);
        confirmed
    }

    /// Confirms each act on a Session of the Remote `remote` not yet
    /// confirmed that a read there, through the Pairing whose key
    /// fingerprint is `pairing`, found it holds: `session_id`, titled
    /// `title` where the read said, which heads its own tree there where
    /// `heads_its_tree`. Answers each beginning this confirmed.
    pub(crate) fn confirm_remote_session(
        &self,
        remote: &str,
        pairing: &str,
        session_id: SessionId,
        title: &str,
        heads_its_tree: bool,
    ) -> Vec<ConfirmedBeginning> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        self.keep_pairing(&mut state, remote, pairing);
        let sidekicks = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, acted_on, act)| *acted_on == session_id && !act.confirmed)
            .map(|(sidekick, ..)| sidekick)
            .collect::<Vec<_>>();
        sidekicks
            .into_iter()
            .filter_map(|sidekick| {
                self.confirm_act(
                    &mut state,
                    sidekick,
                    remote,
                    session_id,
                    Some(title),
                    heads_its_tree,
                )
            })
            .collect()
    }

    /// Confirms each act on a Session of the Remote `remote` not yet
    /// confirmed whose Session is among `listed`, its listing's Sessions by
    /// the Title each is given there, where it is given one. A listing holds
    /// top-level Sessions alone, so each heads its own tree.
    fn confirm_listed(
        &self,
        state: &mut SessionStoreState,
        remote: &str,
        listed: &HashMap<SessionId, Option<String>>,
    ) -> Vec<ConfirmedBeginning> {
        let unconfirmed = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, acted_on, act)| !act.confirmed && listed.contains_key(acted_on))
            .map(|(sidekick, acted_on, _)| (sidekick, acted_on))
            .collect::<Vec<_>>();
        unconfirmed
            .into_iter()
            .filter_map(|(sidekick, session_id)| {
                let title = listed.get(&session_id).cloned().flatten();
                self.confirm_act(state, sidekick, remote, session_id, title.as_deref(), true)
            })
            .collect()
    }

    /// Confirms the act of the Sidekick of `sidekick` on the Session
    /// `session_id` of the Remote `remote`, which a read there found it
    /// holds, here and in storage, answering the beginning this confirmed
    /// where it was one — named by `title`, where the read gave one, and
    /// otherwise by what it was first asked.
    fn confirm_act(
        &self,
        state: &mut SessionStoreState,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
        title: Option<&str>,
        heads_its_tree: bool,
    ) -> Option<ConfirmedBeginning> {
        let origin = Outlook::Remote(remote.to_owned());
        let acted_on = SessionReference::new(origin.clone(), session_id);
        let act = state.sidekick_acts.get_mut(sidekick, &acted_on)?;
        if act.confirmed {
            return None;
        }
        act.confirmed = true;
        act.resolved |= heads_its_tree;
        let beginning = act.beginning.take();
        let stored = crate::storage::StoredSidekickAct {
            sidekick,
            origin,
            session_id,
            acted_at: act.acted_at,
            began: act.began,
            resolved: act.resolved,
            confirmed: true,
            pairing: act.pairing.clone(),
            beginning: None,
        };
        let began = act.began;
        self.storage.record_sidekick_act(stored);
        state.announce_tree_headed_by(sidekick);
        let prompt = beginning.map(|beginning| beginning.create.prompt.text)?;
        began.then(|| ConfirmedBeginning {
            sidekick,
            session_id,
            title: title.map_or_else(|| prompt.clone(), str::to_owned),
            prompt,
        })
    }

    /// Forgets every act on a Session of the Remote `remote` carried through
    /// a Pairing other than the one whose key fingerprint is `pairing`, here
    /// and in storage: the name is paired anew, to another key, so what was
    /// done through the old Pairing was done on another Server, which this
    /// one no longer reaches by that name.
    fn keep_pairing(&self, state: &mut SessionStoreState, remote: &str, pairing: &str) {
        let other = state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(_, _, act)| !act.pairing.is_empty() && act.pairing != pairing)
            .map(|(sidekick, session_id, _)| (sidekick, session_id))
            .collect::<Vec<_>>();
        for (sidekick, session_id) in other {
            let acted_on = SessionReference::new(Outlook::Remote(remote.to_owned()), session_id);
            if state.sidekick_acts.forget_one(sidekick, &acted_on) {
                state.announce_tree_headed_by(sidekick);
                if let Some(change) = state.note_remote_subsessions(sidekick) {
                    state.publish_catalog_change(change);
                }
                self.storage
                    .forget_sidekick_act(sidekick, remote.to_owned(), session_id);
            }
        }
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
    /// stand by the Session heading them there, each with whether a read of
    /// it finding the Remote holds no such Session drops them: not where one
    /// is a beginning whose creation is not yet asked for, or not yet
    /// answered, which no read before its answer can judge.
    pub(crate) fn unresolved_remote_acts(&self, remote: &str) -> Vec<(SessionId, bool)> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut unresolved = HashMap::<SessionId, bool>::new();
        for (_, session_id, act) in state.sidekick_acts.at_remote(remote) {
            if act.resolved {
                continue;
            }
            let pending = act.began && act.beginning.is_some();
            *unresolved.entry(session_id).or_insert(true) &= !pending;
        }
        let mut unresolved = unresolved.into_iter().collect::<Vec<_>>();
        unresolved.sort_by_key(|(session_id, _)| session_id.as_uuid());
        unresolved
    }

    /// Stands every act on the Session `session_id` of the Remote `remote`,
    /// not yet resolved, by `top_level`, the Session the Remote says heads
    /// it — itself, where it is no Subagent's. Its tree holding it says the
    /// Remote holds it, which confirms an act on it not yet confirmed;
    /// answers each beginning this confirmed.
    pub(crate) fn resolve_remote_acts(
        &self,
        remote: &str,
        session_id: SessionId,
        top_level: SessionId,
    ) -> Vec<ConfirmedBeginning> {
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
            return Vec::new();
        }
        if top_level != session_id {
            state.sidekick_acts.forget_on(&SessionReference::new(
                Outlook::Remote(remote.to_owned()),
                session_id,
            ));
            self.storage
                .forget_remote_sidekick_acts(remote.to_owned(), session_id);
        }
        let mut confirmed = Vec::new();
        for (sidekick, _, act) in unresolved {
            let prompt = (act.began && !act.confirmed)
                .then(|| {
                    act.beginning
                        .as_ref()
                        .map(|beginning| beginning.create.prompt.text.clone())
                })
                .flatten();
            self.land_remote_act(
                &mut state,
                sidekick,
                remote,
                top_level,
                super::sidekick_acts::Act {
                    resolved: true,
                    confirmed: true,
                    beginning: None,
                    ..act
                },
            );
            confirmed.extend(prompt.map(|prompt| ConfirmedBeginning {
                sidekick,
                session_id: top_level,
                title: prompt.clone(),
                prompt,
            }));
        }
        confirmed
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
        state.remote_readings.trees.remove(remote);
        state.remote_readings.unfollowed.remove(remote);
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
        state.remote_readings.trees.remove(remote);
        state.remote_readings.unfollowed.remove(remote);
        state.announce_trees_listing(remote);
    }

    /// Forgets what the Remote `remote` said, now it is kept in view no
    /// longer, so it is read afresh when it is again.
    pub(crate) fn remote_unwatched(&self, remote: &str) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state.remote_readings.by_remote.remove(remote);
        state.remote_readings.trees.remove(remote);
        state.remote_readings.unfollowed.remove(remote);
    }

    /// The Sessions of the Remote `remote` whose trees there are to be
    /// followed now: each it answered it holds that a tree some Client
    /// watches lists.
    pub(crate) fn remote_trees_wanted(&self, remote: &str) -> HashSet<SessionId> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(RemoteReading::Answering(held)) = state.remote_readings.by_remote.get(remote)
        else {
            return HashSet::new();
        };
        state
            .sidekick_acts
            .at_remote(remote)
            .filter(|(sidekick, session_id, act)| {
                act.resolved
                    && held.contains_key(session_id)
                    && state.subagent_trees.is_watched(*sidekick)
            })
            .map(|(_, session_id, _)| session_id)
            .collect()
    }

    /// Takes up `tree`, the tree the Session `session_id` of the Remote
    /// `remote` heads there, as that Remote says of it now — where the
    /// Remote answers and holds it.
    /// What is kept of it is its own branch, within `bounds`.
    pub(crate) fn remote_tree_read(
        &self,
        remote: &str,
        session_id: SessionId,
        mut tree: SubagentTreeSnapshot,
        bounds: TreeBounds,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if !state.remote_holds(remote, session_id) || branch_work(&tree, session_id).is_none() {
            return;
        }
        let cut = keep_branch(&mut tree, session_id, bounds);
        state
            .remote_readings
            .trees
            .entry(remote.to_owned())
            .or_default()
            .insert(session_id, RemoteTree { tree, cut });
        state.announce_trees_listing(remote);
    }

    /// Takes up that the trees the Sessions `unfollowed` of the Remote
    /// `remote` head there are not followed, for want of room among the
    /// trees this Server follows.
    pub(crate) fn remote_trees_unfollowed(&self, remote: &str, unfollowed: HashSet<SessionId>) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let held = state
            .remote_readings
            .unfollowed
            .entry(remote.to_owned())
            .or_default();
        if *held == unfollowed {
            return;
        }
        *held = unfollowed;
        state.announce_trees_listing(remote);
    }

    /// Takes up one change the Remote `remote` said of the tree its Session
    /// `session_id` heads there, as its own subscription said it.
    pub(crate) fn remote_tree_changed(
        &self,
        remote: &str,
        session_id: SessionId,
        change: SubagentTreeChange,
        bounds: TreeBounds,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(held) = state
            .remote_readings
            .trees
            .get_mut(remote)
            .and_then(|trees| trees.get_mut(&session_id))
        else {
            return;
        };
        held.tree.apply(change);
        held.cut |= keep_branch(&mut held.tree, session_id, bounds);
        state.announce_trees_listing(remote);
    }

    /// Forgets the tree the Session `session_id` of the Remote `remote`
    /// heads there, no longer followed, so nothing it said stands as
    /// current.
    pub(crate) fn remote_tree_dropped(&self, remote: &str, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let dropped = state
            .remote_readings
            .trees
            .get_mut(remote)
            .and_then(|trees| trees.remove(&session_id));
        if dropped.is_some() {
            state.announce_trees_listing(remote);
        }
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
        if let Some(trees) = state.remote_readings.trees.get_mut(remote) {
            trees.remove(&session_id);
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
    /// Whether the Remote `remote` answers now, holding its Session
    /// `session_id`.
    fn remote_holds(&self, remote: &str, session_id: SessionId) -> bool {
        matches!(
            self.remote_readings.by_remote.get(remote),
            Some(RemoteReading::Answering(held)) if held.contains_key(&session_id)
        )
    }

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
    /// Remote last said of them, by what last said which each is where it
    /// does not answer now, and none for a Remote not read since it was kept
    /// in view, or a Session it was not found to hold. One the Sidekick began
    /// there is its Subsession, whose tree is the Sidekick's here.
    pub(super) fn remote_sessions_beneath(&self, sidekick: SessionId) -> Vec<SubagentTreeSession> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        self.sidekick_acts
            .everywhere_of(sidekick)
            .filter(|(_, act)| act.resolved)
            .filter_map(|(acted_on, act)| {
                let remote = acted_on.origin.remote_name()?;
                let (title, workspace_path, workspace_icon) =
                    match self.remote_readings.by_remote.get(remote)? {
                        RemoteReading::Answering(held) => match held.get(&acted_on.session_id) {
                            Some(summary) => {
                                return Some(SubagentTreeSession {
                                    subagents_unshown: self
                                        .remote_subagents_unshown(remote, acted_on.session_id),
                                    ..remote_session(
                                        remote,
                                        summary,
                                        &act,
                                        self.remote_tree(remote, acted_on.session_id),
                                    )
                                });
                            }
                            // An act not yet confirmed whose Session the Remote
                            // does not list yet stands by what it asked for,
                            // until a read finds it, or finds it is not there.
                            None if !act.confirmed => (None, None, None),
                            None => return None,
                        },
                        RemoteReading::Silent(last) => {
                            let last = last.get(&acted_on.session_id);
                            (
                                last.map(|summary| summary.title.clone()),
                                last.map(|summary| summary.session.workspace.path.clone()),
                                last.and_then(|summary| summary.session.workspace.icon.clone()),
                            )
                        }
                    };
                let asked = act.beginning.as_ref();
                Some(SubagentTreeSession {
                    subagents_unshown: false,
                    unconfirmed: !act.confirmed,
                    session_id: acted_on.session_id,
                    origin: Some(remote.to_owned()),
                    unanswered: matches!(
                        self.remote_readings.by_remote.get(remote),
                        Some(RemoteReading::Silent(_))
                    ),
                    title: title
                        .or_else(|| asked.map(|asked| asked.create.prompt.text.clone()))
                        .unwrap_or_default(),
                    subsession: act.began,
                    workspace_path: workspace_path
                        .or_else(|| asked.map(|asked| asked.directory.clone()))
                        .unwrap_or_default(),
                    workspace_icon,
                    model: None,
                    status: None,
                    worked_ms: None,
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
                    acted_at: act.acted_at,
                })
            })
            .collect()
    }

    /// The Subagents beneath each Session the Sidekick's Session `sidekick`
    /// acted on at an answering Remote, as the tree it heads there last said
    /// of them, each named by that Remote: none where that tree is not
    /// followed now.
    pub(super) fn remote_subagents_beneath(&self, sidekick: SessionId) -> Vec<SubagentTreeEntry> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        self.sidekick_acts
            .everywhere_of(sidekick)
            .filter(|(_, act)| act.resolved)
            .filter_map(|(acted_on, _)| {
                let remote = acted_on.origin.remote_name()?;
                self.remote_holds(remote, acted_on.session_id)
                    .then(|| self.remote_tree(remote, acted_on.session_id))
                    .flatten()
                    .map(|tree| ((remote, acted_on.session_id), tree))
            })
            .flat_map(|((remote, session_id), tree)| {
                branch_of(tree, session_id)
                    .into_iter()
                    .map(move |entry| SubagentTreeEntry {
                        origin: Some(remote.to_owned()),
                        ..entry.clone()
                    })
            })
            .collect()
    }

    /// The tree the Session `session_id` of the Remote `remote` heads there,
    /// as that Remote last said of it, where it is followed.
    fn remote_tree(&self, remote: &str, session_id: SessionId) -> Option<&SubagentTreeSnapshot> {
        self.remote_readings
            .trees
            .get(remote)?
            .get(&session_id)
            .map(|held| &held.tree)
    }

    /// Whether some of the Subagents beneath the Session `session_id` of the
    /// Remote `remote` are not shown: its tree there ran past what is kept
    /// of one, or is not followed for want of room.
    fn remote_subagents_unshown(&self, remote: &str, session_id: SessionId) -> bool {
        self.remote_readings
            .trees
            .get(remote)
            .and_then(|trees| trees.get(&session_id))
            .is_some_and(|held| held.cut)
            || self
                .remote_readings
                .unfollowed
                .get(remote)
                .is_some_and(|unfollowed| unfollowed.contains(&session_id))
    }
}

/// Where a Session's work stands: its Marker, how long its settled Turns
/// worked, when the work it does now began, and when it began Monitoring.
type BranchWork = (
    Option<ActivityStatus>,
    Option<u64>,
    Option<SessionTimestamp>,
    Option<SessionTimestamp>,
);

/// Where the work of the Session `session_id` stands, as `tree` — what its
/// Remote answered for it — says, or `None` where the tree does not hold its
/// branch. A Session that Remote's own Sidekick began heads no tree there,
/// the Remote answering for it with its Sidekick's, where it stands as a
/// Subsession.
fn branch_work(tree: &SubagentTreeSnapshot, session_id: SessionId) -> Option<BranchWork> {
    if tree.top_level.session_id == session_id {
        return Some((
            tree.top_level.status,
            tree.top_level.worked_ms,
            tree.top_level.own_working_since,
            tree.top_level.monitoring_since,
        ));
    }
    tree.sessions
        .iter()
        .find(|held| held.origin.is_none() && held.session_id == session_id && held.subsession)
        .map(|held| {
            (
                held.status,
                held.worked_ms,
                held.working_since,
                held.monitoring_since,
            )
        })
}

/// Keeps of `tree`, the tree the Session `session_id` heads at its Remote or
/// stands in there, nothing but that Session and its own branch, within
/// `bounds`: no Subagent deeper beneath it than they allow, and no more of
/// them, the earliest kept. Answers whether any of its own branch was left
/// out for running past them.
fn keep_branch(tree: &mut SubagentTreeSnapshot, session_id: SessionId, bounds: TreeBounds) -> bool {
    tree.sessions
        .retain(|held| held.origin.is_none() && held.session_id == session_id);
    let parents = tree
        .subagents
        .iter()
        .filter(|entry| entry.origin.is_none())
        .map(|entry| (entry.session_id, entry.parent_session_id))
        .collect::<HashMap<_, _>>();
    // How deep beneath the Session each Subagent of its branch stands.
    let depth = |entry: &SubagentTreeEntry| {
        let mut at = entry.session_id;
        for depth in 1..=parents.len() {
            match parents.get(&at) {
                Some(parent) if *parent == session_id => return Some(depth),
                Some(parent) => at = *parent,
                None => return None,
            }
        }
        None
    };
    let mut kept = 0;
    let mut cut = false;
    tree.subagents.retain(|entry| {
        if entry.origin.is_some() {
            return false;
        }
        let Some(depth) = depth(entry) else {
            return false;
        };
        if depth > bounds.depth || kept == bounds.entries {
            cut = true;
            return false;
        }
        kept += 1;
        true
    });
    cut
}

/// The Subagents of `tree` beneath the Session `session_id`, at any depth:
/// those of its own branch, where the tree is its Sidekick's there, and
/// nothing of another Server's.
fn branch_of(tree: &SubagentTreeSnapshot, session_id: SessionId) -> Vec<&SubagentTreeEntry> {
    let parents = tree
        .subagents
        .iter()
        .filter(|entry| entry.origin.is_none())
        .map(|entry| (entry.session_id, entry.parent_session_id))
        .collect::<HashMap<_, _>>();
    let beneath = |entry: &SubagentTreeEntry| {
        let mut at = entry.session_id;
        // A Subagent cannot be its own ancestor, so a walk longer than the
        // tree is one round a cycle, and ends there.
        for _ in 0..=parents.len() {
            match parents.get(&at) {
                Some(parent) if *parent == session_id => return true,
                Some(parent) => at = *parent,
                None => return false,
            }
        }
        false
    };
    tree.subagents
        .iter()
        .filter(|entry| entry.origin.is_none() && beneath(entry))
        .collect()
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
/// it, `act`. Its work is read from `tree`, the tree it heads there, where
/// that is followed, as an entry for a Session of this Server's reads it;
/// and otherwise from its listing, which says when it works and how its
/// latest Turn settled, and nothing of how long its settled Turns took, so
/// its time is left unsaid once it settles. One the Sidekick began there is
/// its Subsession.
fn remote_session(
    remote: &str,
    summary: &SessionSummary,
    act: &super::sidekick_acts::Act,
    tree: Option<&SubagentTreeSnapshot>,
) -> SubagentTreeSession {
    let session = &summary.session;
    let listed_status = if session.working_since.is_some() {
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
    let (status, worked_ms, working_since, monitoring_since) =
        match tree.and_then(|tree| branch_work(tree, session.id)) {
            Some(work) => work,
            None => (
                listed_status,
                session.working_since.map(|_| 0),
                session.working_since,
                session.monitoring_since,
            ),
        };
    SubagentTreeSession {
        subagents_unshown: false,
        unconfirmed: !act.confirmed,
        session_id: session.id,
        origin: Some(remote.to_owned()),
        unanswered: false,
        title: summary.title.clone(),
        subsession: act.began,
        workspace_path: session.workspace.path.clone(),
        workspace_icon: session.workspace.icon.clone(),
        model: session
            .agent_selection
            .as_ref()
            .map(|selection| selection.model.clone()),
        status,
        worked_ms,
        working_since,
        monitoring_since,
        needs_intervention: !summary.standing_inputs.pending_questionnaires.is_empty()
            || !summary.standing_inputs.pending_approvals.is_empty(),
        acted_at: act.acted_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::sidekick_acts::Act;

    fn act(acted_at: u64, resolved: bool) -> Act {
        Act {
            beginning: None,
            confirmed: true,
            pairing: String::new(),
            acted_at: SessionTimestamp(acted_at),
            began: false,
            resolved,
        }
    }

    fn subagent(session_id: SessionId, parent_session_id: SessionId) -> SubagentTreeEntry {
        SubagentTreeEntry {
            session_id,
            origin: None,
            parent_session_id,
            spawn_order: 0,
            name: "Explore".to_owned(),
            title: "Explore".to_owned(),
            model: None,
            status: ActivityStatus::Active,
            worked_ms: None,
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
        }
    }

    /// The tree a Remote answers for its Sidekick's Session `sidekick`, in
    /// which `subsession` stands, with `subagents` beneath them.
    fn sidekicks_tree(
        sidekick: SessionId,
        subsession: SessionId,
        subagents: Vec<SubagentTreeEntry>,
    ) -> SubagentTreeSnapshot {
        SubagentTreeSnapshot {
            revision: crate::protocol::SubagentTreeRevision::INITIAL,
            top_level: crate::protocol::SubagentTreeTopLevel {
                session_id: sidekick,
                title: "Plan the week".to_owned(),
                working_since: None,
                monitoring_since: None,
                status: None,
                worked_ms: None,
                own_working_since: None,
                needs_intervention: false,
                sidekick: true,
            },
            subagents,
            sessions: vec![SubagentTreeSession {
                session_id: subsession,
                origin: None,
                unanswered: false,
                unconfirmed: false,
                subagents_unshown: false,
                title: "Fix the parser".to_owned(),
                subsession: true,
                workspace_path: std::path::PathBuf::new(),
                workspace_icon: None,
                model: None,
                status: Some(ActivityStatus::Active),
                worked_ms: Some(0),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                acted_at: SessionTimestamp(1),
            }],
        }
    }

    /// What is kept of a Remote Session's tree is its own branch alone, and
    /// of that no more Subagents, nor any deeper, than the bounds allow —
    /// the earliest kept, and the tree marked as cut.
    #[test]
    fn a_remote_sessions_tree_keeps_its_own_branch_within_bounds() {
        let (sidekick, subsession) = (SessionId::new(), SessionId::new());
        let (first, second, beneath_first, of_the_sidekick) = (
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
        );
        let tree = sidekicks_tree(
            sidekick,
            subsession,
            vec![
                subagent(of_the_sidekick, sidekick),
                subagent(first, subsession),
                subagent(beneath_first, first),
                subagent(second, subsession),
            ],
        );
        let kept = |bounds: TreeBounds| {
            let mut tree = tree.clone();
            let cut = keep_branch(&mut tree, subsession, bounds);
            let ids = tree
                .subagents
                .iter()
                .map(|entry| entry.session_id)
                .collect::<Vec<_>>();
            (ids, cut, tree.sessions.len())
        };
        assert_eq!(
            kept(TreeBounds {
                entries: 8,
                depth: 8
            }),
            (vec![first, beneath_first, second], false, 1),
            "its own branch alone, and nothing of its Sidekick's other work"
        );
        assert_eq!(
            kept(TreeBounds {
                entries: 8,
                depth: 1
            }),
            (vec![first, second], true, 1),
            "none deeper than the bounds allow"
        );
        assert_eq!(
            kept(TreeBounds {
                entries: 2,
                depth: 8
            }),
            (vec![first, beneath_first], true, 1),
            "and no more of them, the earliest kept"
        );
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
