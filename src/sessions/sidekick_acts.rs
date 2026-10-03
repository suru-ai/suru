//! The Sessions each Sidekick has a hand in: its Subsessions, and every other
//! Session it has acted on — sent a Prompt, answered, interrupted, set aside,
//! or brought back (CONTEXT.md: Subagents Section) — on this Server or on a
//! Remote, since only the Sidekick's own Server knows both ends of an act on a
//! Remote's Session. Reading a Session is no act.
//!
//! Each act is recorded against the Sidekick's Session, naming the Session it
//! acted on — a Subagent's own, where it acted on one — and the moment of its
//! latest act on that Session. The record is kept across a stop, landing in
//! the same write as the change the act made where it made one, and is
//! forgotten when either Session is deleted. What the record is for is the
//! tree a Client is given for a Sidekick's Session: it lists, beneath the
//! Sidekick's own Subagents, the top-level Session heading each Session acted
//! on, each with its own Subagents beneath it, and a Subsession's tree is its
//! Sidekick's. A Session only acted on heads its own tree, unchanged, since
//! several Sidekicks may have acted on it.
//!
//! A listed Session is drawn from what was stored of it without reading its
//! history (ADR 0022): its summary and Turns, which a restored Session holds
//! from startup, and the rows recording the Subagents it spawned, read alone
//! (see [`SessionStore::prepare_subagent_tree`]). Its history is read only
//! when it is opened or acted on.
//!
//! None of it makes those Sessions the Sidekick's in any way that matters to
//! their work: nothing here rolls their Working, Usage or Cost up beneath it,
//! and nothing reaches them when its Session is interrupted or deleted.
//!
//! A Remote's Session is drawn from what that Remote says of it now, read
//! while some Client watches a tree listing it (see
//! [`super::remote_sessions`]), and a Remote's Session found deleted is
//! forgotten when it is so read.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{
    Activity, CreateSessionRequest, Outlook, PrepareCheckoutRequest, RemoteSession,
    SessionCatalogChange, SessionChange, SessionId, SessionReference, SessionTimestamp,
};
use crate::sidekick::SidekickWorkspace;
use crate::storage::{StorageError, StoredSidekickAct};

use super::{SessionStore, SessionStoreState};

/// Every Sidekick's latest act on each Session it has acted on, by the
/// Sidekick's Session and then the Session acted on, with its Origin.
#[derive(Default)]
pub(super) struct SidekickActs {
    by_sidekick: HashMap<SessionId, HashMap<SessionReference, Act>>,
}

/// A Sidekick's acts on one Session: the moment of its latest, whether it
/// began the Session, whether the Session is known to head its own tree,
/// whether the act is known to have been done, the Pairing it was carried
/// through, and what a beginning not yet confirmed asks for — all but the
/// first kept for a Remote's alone, since only this Server knows it began
/// there, only the Remote can say what heads a Subagent's Session, and only
/// an act carried to a Remote can have an outcome not yet learned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Act {
    pub(crate) acted_at: SessionTimestamp,
    pub(crate) began: bool,
    pub(crate) resolved: bool,
    /// Whether the act is known to have been done: false for one carried to
    /// a Remote whose answer never came back whole, until a read of that
    /// Remote shows the Session it named. Once so, always so.
    pub(crate) confirmed: bool,
    /// The key fingerprint of the Pairing the act was carried through, and
    /// empty for an act of this Server's own.
    pub(crate) pairing: String,
    /// What a beginning on a Remote not yet confirmed asks for, so asking
    /// again is the very same request.
    pub(crate) beginning: Option<Beginning>,
    /// What a read of the Remote must find to confirm it, where it is not
    /// yet confirmed.
    pub(crate) evidence: ActEvidence,
}

/// What a read of a Remote must find to confirm an act there not yet
/// confirmed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActEvidence {
    /// The Session it named, standing there: all an act that leaves nothing
    /// behind — interrupting, setting aside or bringing back — needs, and a
    /// beginning, which named its Session beforehand; and all a confirmed
    /// act ever does.
    #[default]
    SessionStanding,
    /// What it left there, one of which a read must find as this Peer's: a
    /// Prompt this Server named, or an Answer it gave as an act it named. A
    /// read asked for after it that finds none finds it was never done. Left
    /// empty where what it left is unavailable — written so no longer read —
    /// so no read confirms it, and the first of its whole tree lets it go.
    Left(Vec<super::RemoteContribution>),
}

impl ActEvidence {
    /// As storage keeps it.
    pub(crate) fn stored(&self) -> String {
        serde_json::to_string(self).expect("an act's evidence always serializes")
    }

    /// As storage kept it: unavailable, where it is no longer read.
    fn from_stored(stored: &str) -> Self {
        serde_json::from_str(stored).unwrap_or(Self::Left(Vec::new()))
    }

    /// What both `self` and `other`, acts on one Session neither yet
    /// confirmed, ask: the Session standing, where either asks no more, and
    /// otherwise anything either left.
    fn with(self, other: Self) -> Self {
        match (self, other) {
            (Self::Left(mut left), Self::Left(more)) => {
                for evidence in more {
                    if !left.contains(&evidence) {
                        left.push(evidence);
                    }
                }
                Self::Left(left)
            }
            _ => Self::SessionStanding,
        }
    }
}

/// What a beginning on a Remote asks for, by the identities chosen before it
/// was first asked — the Session's own, its first Prompt's, and its Worktree
/// preparation's — so asking again is the very same request: a creation the
/// Remote already took answers with the Session it made, and a preparation
/// it already made resumes. Kept until the beginning is confirmed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Beginning {
    /// The directory the Sidekick named to begin it in, as it named it.
    pub(crate) directory: PathBuf,
    /// The Worktree preparation asked for first, where one is.
    pub(crate) prepare: Option<PrepareCheckoutRequest>,
    /// The creation asked for: in the prepared Worktree once the
    /// preparation answered where that is.
    pub(crate) create: CreateSessionRequest,
    /// Whether the creation was asked for, so the Remote may have taken it:
    /// asked again from here, only the creation is asked for, never the
    /// preparation, which a Session it began no longer needs.
    pub(crate) creating: bool,
    /// The name the Sidekick was given its Worktree preparation by, where
    /// it asked for one, so a Worktree the Remote kept is named to it again.
    pub(crate) preparation_named: Option<String>,
}

impl Beginning {
    /// The identity the Session has where it was begun.
    pub(crate) fn session_id(&self) -> SessionId {
        self.create
            .session_id
            .expect("a beginning on a Remote chooses its Session's identity first")
    }
}

impl Act {
    /// An act of this Server's own at `acted_at`, which is always known to
    /// have been done.
    pub(super) const fn here(acted_at: SessionTimestamp) -> Self {
        Self {
            acted_at,
            began: false,
            resolved: true,
            confirmed: true,
            pairing: String::new(),
            beginning: None,
            evidence: ActEvidence::SessionStanding,
        }
    }

    /// Takes up `recorded`, a later act on the same Session. An act not
    /// known to have been done moves nothing an act that was already says:
    /// it may never have happened.
    fn take_up(&mut self, recorded: Act) {
        if self.confirmed && !recorded.confirmed {
            return;
        }
        self.acted_at = self.acted_at.max(recorded.acted_at);
        self.began |= recorded.began;
        self.resolved |= recorded.resolved;
        self.confirmed |= recorded.confirmed;
        if !recorded.pairing.is_empty() {
            self.pairing = recorded.pairing;
        }
        self.beginning = if self.confirmed {
            None
        } else {
            recorded.beginning.or(self.beginning.take())
        };
        self.evidence = if self.confirmed {
            ActEvidence::SessionStanding
        } else {
            std::mem::take(&mut self.evidence).with(recorded.evidence)
        };
    }
}

/// An act on a Remote's Session to record: whether it began the Session,
/// whether the Session is known to head its own tree there, whether the act
/// is known to have been done, the Pairing it was carried through, and what
/// a beginning not yet confirmed asks for.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RemoteAct {
    pub(crate) began: bool,
    pub(crate) resolved: bool,
    pub(crate) confirmed: bool,
    pub(crate) pairing: String,
    pub(crate) beginning: Option<Beginning>,
    /// What it left there to be found by, where it is not yet confirmed.
    pub(crate) evidence: Option<super::RemoteContribution>,
}

impl SidekickActs {
    fn record(&mut self, sidekick: SessionId, acted_on: SessionReference, recorded: Act) {
        match self
            .by_sidekick
            .entry(sidekick)
            .or_default()
            .entry(acted_on)
        {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                held.get_mut().take_up(recorded)
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(recorded);
            }
        }
    }

    /// The act of the Sidekick of `sidekick` on `acted_on`, where it acted
    /// on it.
    pub(super) fn get(&self, sidekick: SessionId, acted_on: &SessionReference) -> Option<&Act> {
        self.by_sidekick.get(&sidekick)?.get(acted_on)
    }

    /// The act of the Sidekick of `sidekick` on `acted_on`, to change in
    /// place.
    pub(super) fn get_mut(
        &mut self,
        sidekick: SessionId,
        acted_on: &SessionReference,
    ) -> Option<&mut Act> {
        self.by_sidekick.get_mut(&sidekick)?.get_mut(acted_on)
    }

    /// Forgets the act of the Sidekick of `sidekick` on `acted_on` alone,
    /// answering whether it had acted on it.
    pub(super) fn forget_one(&mut self, sidekick: SessionId, acted_on: &SessionReference) -> bool {
        let Some(acts) = self.by_sidekick.get_mut(&sidekick) else {
            return false;
        };
        let forgotten = acts.remove(acted_on).is_some();
        if acts.is_empty() {
            self.by_sidekick.remove(&sidekick);
        }
        forgotten
    }

    /// Every act on a Session of the Remote `remote`, as (the Sidekick's
    /// Session, the Session acted on there, the act).
    pub(super) fn at_remote<'a>(
        &'a self,
        remote: &'a str,
    ) -> impl Iterator<Item = (SessionId, SessionId, Act)> + 'a {
        self.by_sidekick
            .iter()
            .flat_map(move |(sidekick, acted_on)| {
                acted_on
                    .iter()
                    .filter(move |(acted_on, _)| acted_on.origin.remote_name() == Some(remote))
                    .map(move |(acted_on, act)| (*sidekick, acted_on.session_id, act.clone()))
            })
    }

    /// The Sessions the Sidekick of `sidekick` began on Remotes, in a stable
    /// order.
    pub(super) fn remote_subsessions_of(&self, sidekick: SessionId) -> Vec<RemoteSession> {
        let mut began = self
            .by_sidekick
            .get(&sidekick)
            .into_iter()
            .flatten()
            .filter(|(_, act)| act.began)
            .filter_map(|(acted_on, _)| {
                Some(RemoteSession {
                    origin: acted_on.origin.remote_name()?.to_owned(),
                    session_id: acted_on.session_id,
                })
            })
            .collect::<Vec<_>>();
        began.sort_by(|left, right| {
            (&left.origin, left.session_id.as_uuid())
                .cmp(&(&right.origin, right.session_id.as_uuid()))
        });
        began
    }

    /// Each Session of this Server the Sidekick of `sidekick` acted on, with
    /// the moment of its latest act on it.
    fn of(&self, sidekick: SessionId) -> impl Iterator<Item = (SessionId, SessionTimestamp)> + '_ {
        self.everywhere_of(sidekick)
            .filter(|(acted_on, _)| acted_on.origin == Outlook::Local)
            .map(|(acted_on, act)| (acted_on.session_id, act.acted_at))
    }

    /// Each Session the Sidekick of `sidekick` acted on, on this Server or a
    /// Remote, with its acts on it.
    pub(super) fn everywhere_of(
        &self,
        sidekick: SessionId,
    ) -> impl Iterator<Item = (&SessionReference, Act)> + '_ {
        self.by_sidekick
            .get(&sidekick)
            .into_iter()
            .flatten()
            .map(|(acted_on, act)| (acted_on, act.clone()))
    }

    /// Every act, as (the Sidekick's Session, the Session it acted on).
    pub(super) fn all(&self) -> impl Iterator<Item = (SessionId, &SessionReference)> + '_ {
        self.by_sidekick.iter().flat_map(|(sidekick, acted_on)| {
            acted_on.keys().map(move |acted_on| (*sidekick, acted_on))
        })
    }

    /// Forgets every act of the Sidekick of `session_id`, and every act on
    /// it, as the deletion of its Session does.
    pub(super) fn forget(&mut self, session_id: SessionId) {
        self.forget_on(&SessionReference::new(Outlook::Local, session_id));
        self.by_sidekick.remove(&session_id);
    }

    /// Forgets every act on `acted_on`, answering each Sidekick that had
    /// acted on it.
    pub(super) fn forget_on(&mut self, acted_on: &SessionReference) -> Vec<SessionId> {
        let mut forgotten = Vec::new();
        for (sidekick, acts) in &mut self.by_sidekick {
            if acts.remove(acted_on).is_some() {
                forgotten.push(*sidekick);
            }
        }
        self.by_sidekick.retain(|_, acted_on| !acted_on.is_empty());
        forgotten
    }

    /// The latest moment any act was recorded at.
    fn latest(&self) -> Option<SessionTimestamp> {
        self.by_sidekick
            .values()
            .flat_map(HashMap::values)
            .map(|act| act.acted_at)
            .max()
    }
}

impl SessionStore {
    /// Knows Sidekicks' Sessions by `workspace`, and takes up every act
    /// `acts` read back from storage. The server always chains this; a test
    /// that omits it holds no Sidekick, so every tree it reads is its own.
    pub(crate) fn with_sidekicks(
        self,
        workspace: SidekickWorkspace,
        acts: Vec<StoredSidekickAct>,
    ) -> Self {
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let mut sidekicks = HashSet::new();
            for act in acts {
                sidekicks.insert(act.sidekick);
                state.sidekick_acts.record(
                    act.sidekick,
                    SessionReference::new(act.origin, act.session_id),
                    Act {
                        acted_at: act.acted_at,
                        began: act.began,
                        resolved: act.resolved,
                        confirmed: act.confirmed,
                        pairing: act.pairing,
                        // One no longer read as written is no beginning to
                        // ask again; the act stands all the same.
                        beginning: act
                            .beginning
                            .and_then(|written| serde_json::from_str(&written).ok()),
                        evidence: ActEvidence::from_stored(&act.evidence),
                    },
                );
            }
            // The store minted every act's moment, so its clock resumes past
            // them as it does past every other moment it minted.
            state.last_timestamp = state.last_timestamp.max(state.sidekick_acts.latest());
            state.sidekick_workspace = Some(workspace);
            // Restored before the Sidekick Workspace was known, so its
            // Sessions have yet to wear its standing Icon.
            let state = &mut *state;
            let sidekick = state.sidekick_workspace.as_ref();
            for record in state.sessions.values_mut() {
                for session in [&mut record.snapshot.session, &mut record.summary.session] {
                    super::dress_workspace(&state.workspaces, sidekick, &mut session.workspace);
                }
            }
            for unreadable in state.unreadable_sessions.values_mut() {
                if let Some(workspace) = &mut unreadable.summary.workspace {
                    super::dress_workspace(&state.workspaces, sidekick, workspace);
                }
            }
            for sidekick in sidekicks {
                state.note_remote_subsessions(sidekick);
            }
        }
        self
    }

    /// Records that the Sidekick of `sidekick` just acted on `session_id`
    /// where the act changed nothing the Session stores — an interrupt,
    /// whose work its Provider settles later, or a Session set aside that
    /// already was — so the record is written on its own.
    pub(crate) fn record_sidekick_act(&self, sidekick: SessionId, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(act) = state.note_sidekick_act(sidekick, session_id) {
            self.storage.record_sidekick_act(act);
        }
    }

    /// Records that the Sidekick of `sidekick` just acted on the Session
    /// `session_id` of the Remote `remote` as `act` says — began it there,
    /// or acted on it; through which Pairing; and whether that is known to
    /// have been done — so it stands beneath the Sidekick's Session in its
    /// tree from now on, ordered by this act, while both Sessions exist. A
    /// Session begun there is one of the Sidekick's Subsessions, which its
    /// Session's summary names for every Client. One the Remote did not say
    /// heads its own tree, not `resolved`, is kept until it does, and stands
    /// in no tree meanwhile; one not known to have been done stands as such
    /// until a read of that Remote confirms or drops it. Nothing is recorded
    /// where the Sidekick's Session is no longer held.
    pub(crate) fn record_remote_sidekick_act(
        &self,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
        act: RemoteAct,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let acted_at = state.next_timestamp();
        self.land_remote_act(
            &mut state,
            sidekick,
            remote,
            session_id,
            Act {
                acted_at,
                began: act.began,
                resolved: act.resolved,
                confirmed: act.confirmed,
                pairing: act.pairing,
                beginning: act.beginning,
                evidence: act.evidence.map_or(ActEvidence::SessionStanding, |left| {
                    ActEvidence::Left(vec![left])
                }),
            },
        );
    }

    /// The act of the Sidekick of `sidekick` on the Session `session_id` of
    /// the Remote `remote`, where it acted on it.
    pub(crate) fn remote_sidekick_act(
        &self,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
    ) -> Option<Act> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sidekick_acts
            .get(
                sidekick,
                &SessionReference::new(Outlook::Remote(remote.to_owned()), session_id),
            )
            .cloned()
    }

    /// Forgets the act of the Sidekick of `sidekick` on the Session
    /// `session_id` of the Remote `remote` alone — a beginning the Remote
    /// refused, say — here and in storage, where nothing else stands by it.
    pub(crate) fn forget_remote_sidekick_act(
        &self,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let acted_on = SessionReference::new(Outlook::Remote(remote.to_owned()), session_id);
        if !state.sidekick_acts.forget_one(sidekick, &acted_on) {
            return;
        }
        state.announce_tree_headed_by(sidekick);
        if let Some(change) = state.note_remote_subsessions(sidekick) {
            state.publish_catalog_change(change);
        }
        self.storage
            .forget_sidekick_act(sidekick, remote.to_owned(), session_id);
    }

    /// Records the act `act` of the Sidekick of `sidekick` on the Session
    /// `session_id` of the Remote `remote`, here and in storage, telling
    /// every Client what it moves.
    pub(super) fn land_remote_act(
        &self,
        state: &mut SessionStoreState,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
        act: Act,
    ) {
        if !state.sessions.contains_key(&sidekick) {
            return;
        }
        let origin = Outlook::Remote(remote.to_owned());
        let acted_on = SessionReference::new(origin.clone(), session_id);
        state.sidekick_acts.record(sidekick, acted_on.clone(), act);
        // Stored as it stands once taken up, so storage holds what memory
        // does.
        let Some(act) = state.sidekick_acts.get(sidekick, &acted_on).cloned() else {
            return;
        };
        state.announce_tree_headed_by(sidekick);
        if let Some(change) = state.note_remote_subsessions(sidekick) {
            state.publish_catalog_change(change);
        }
        self.storage.record_sidekick_act(StoredSidekickAct {
            sidekick,
            origin,
            session_id,
            acted_at: act.acted_at,
            began: act.began,
            resolved: act.resolved,
            confirmed: act.confirmed,
            pairing: act.pairing,
            beginning: act.beginning.map(|beginning| {
                serde_json::to_string(&beginning).expect("a beginning always serializes")
            }),
            evidence: act.evidence.stored(),
        });
    }

    /// Brings into memory what the tree `session_id` belongs to needs to be
    /// drawn: the history of `session_id` itself, which a reader is opening,
    /// and, for each Session of the tree not yet read — the Sidekick's
    /// Session heading it where `session_id` is a Subsession, each Session
    /// that Sidekick has a hand in, and every Subagent beneath them — the
    /// stored rows recording the Subagents it spawned, and nothing else of
    /// its history.
    pub(crate) async fn prepare_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> Result<(), StorageError> {
        self.hydrate(session_id).await?;
        let (repository, unread) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let Some(top_level) = state.top_level_of(session_id) else {
                return Ok(());
            };
            let head = state.tree_head(top_level);
            let roots = std::iter::once(head)
                .chain(
                    state
                        .sessions_beneath(head)
                        .into_iter()
                        .map(|(listed, _, _)| listed),
                )
                .collect::<Vec<_>>();
            let unread = state
                .subtrees_of(&roots)
                .into_iter()
                .filter(|id| state.is_deferred(*id) && !state.stored_subagent_rows.contains_key(id))
                .collect::<Vec<_>>();
            let Some(deferred) = state.deferred.as_ref() else {
                return Ok(());
            };
            (deferred.repository.clone(), unread)
        };
        if unread.is_empty() {
            return Ok(());
        }
        let mut rows = repository.subagent_rows(unread.clone()).await?;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        for id in unread {
            // One read meanwhile holds its rows in its history instead.
            if state.is_deferred(id) {
                let rows = rows.remove(&id).unwrap_or_default();
                state.stored_subagent_rows.insert(id, rows);
            }
        }
        Ok(())
    }
}

impl SessionStoreState {
    /// Records that the Sidekick of `sidekick` acted on `session_id` just
    /// now, answering the record to store: the Session acted on stands beneath
    /// the Sidekick's in its tree from now on — by the top-level Session
    /// heading its tree, where it is a Subagent's — ordered by this act.
    /// Nothing is recorded where either Session is no longer held, or where
    /// the Session acted on is in the Sidekick's own tree.
    pub(super) fn note_sidekick_act(
        &mut self,
        sidekick: SessionId,
        session_id: SessionId,
    ) -> Option<StoredSidekickAct> {
        let top_level = self.top_level_of(session_id)?;
        if top_level == sidekick || !self.sessions.contains_key(&sidekick) {
            return None;
        }
        let acted_at = self.next_timestamp();
        self.sidekick_acts.record(
            sidekick,
            SessionReference::new(Outlook::Local, session_id),
            Act::here(acted_at),
        );
        self.announce_tree_headed_by(sidekick);
        Some(StoredSidekickAct {
            sidekick,
            origin: Outlook::Local,
            session_id,
            acted_at,
            began: false,
            resolved: true,
            confirmed: true,
            pairing: String::new(),
            beginning: None,
            evidence: ActEvidence::SessionStanding.stored(),
        })
    }

    /// Records the act of each Sidekick whose Answer to a Questionnaire in
    /// `session_id` `changes` take, answering the records to store with that
    /// commit, so each lands with the Answer it records.
    pub(super) fn note_sidekicks_answers(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
    ) -> Vec<StoredSidekickAct> {
        changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::QuestionnaireSettled {
                    answer: Some(_),
                    author: Some(author),
                    ..
                } => author.sidekick_session(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|sidekick| self.note_sidekick_act(sidekick, session_id))
            .collect()
    }

    /// Brings the summary of the Sidekick's Session `sidekick` up to the
    /// Sessions it began on Remotes, answering the change to tell every
    /// Client where they moved.
    pub(super) fn note_remote_subsessions(
        &mut self,
        sidekick: SessionId,
    ) -> Option<SessionCatalogChange> {
        let began = self.sidekick_acts.remote_subsessions_of(sidekick);
        let record = self.sessions.get_mut(&sidekick)?;
        if record.summary.remote_subsessions == began {
            return None;
        }
        record.summary.remote_subsessions.clone_from(&began);
        Some(SessionCatalogChange::RemoteSubsessionsChanged {
            session_id: sidekick,
            remote_subsessions: began,
        })
    }

    /// Whether `session_id` is a Sidekick's Session: a top-level Session of
    /// the Sidekick Workspace.
    pub(super) fn is_sidekicks(&self, session_id: SessionId) -> bool {
        self.sidekick_workspace.as_ref().is_some_and(|workspace| {
            self.sessions
                .get(&session_id)
                .is_some_and(|record| workspace.is_sidekicks(&record.snapshot.session))
        })
    }

    /// The Session heading the tree a Client is given for the top-level
    /// Session `top_level`: its Sidekick's, for a Subsession whose Sidekick's
    /// Session is still held, and its own otherwise.
    pub(super) fn tree_head(&self, top_level: SessionId) -> SessionId {
        self.sessions
            .get(&top_level)
            .and_then(|record| record.summary.session.sidekick())
            .filter(|sidekick| self.is_sidekicks(*sidekick))
            .unwrap_or(top_level)
    }

    /// The Sessions standing beneath the Sidekick's Session `sidekick`, the
    /// one acted on most recently first: the held top-level Session heading
    /// each Session it acted on, by the moment of its latest act on any of
    /// them, and each Subsession it began, by that act or — where none was
    /// recorded — the moment it was begun; and whether it is a Subsession.
    /// Nothing stands beneath a Session that is not a Sidekick's.
    pub(super) fn sessions_beneath(
        &self,
        sidekick: SessionId,
    ) -> Vec<(SessionId, SessionTimestamp, bool)> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        let mut beneath = HashMap::<SessionId, (SessionTimestamp, bool)>::new();
        for (acted_on, acted_at) in self.sidekick_acts.of(sidekick) {
            let Some(top_level) = self.top_level_of(acted_on) else {
                continue;
            };
            let (latest, _) = beneath.entry(top_level).or_insert((acted_at, false));
            *latest = (*latest).max(acted_at);
        }
        for subsession in self.subsessions_of(sidekick) {
            let begun_at = self.sessions[&subsession].summary.created_at;
            beneath
                .entry(subsession)
                .and_modify(|(_, began)| *began = true)
                .or_insert((begun_at, true));
        }
        let mut beneath = beneath
            .into_iter()
            .filter(|(session_id, _)| *session_id != sidekick)
            .map(|(session_id, (acted_at, began))| (session_id, acted_at, began))
            .collect::<Vec<_>>();
        beneath.sort_by_key(|(session_id, acted_at, _)| {
            (
                std::cmp::Reverse(*acted_at),
                std::cmp::Reverse(session_id.as_uuid()),
            )
        });
        beneath
    }

    /// Every tree that lists the top-level Session `top_level`, by the
    /// Session heading it: its own, its Sidekick's where it is a Subsession,
    /// and that of each Sidekick that acted on it or on a Subagent beneath
    /// it.
    pub(super) fn trees_listing(&self, top_level: SessionId) -> Vec<SessionId> {
        let mut trees = vec![top_level];
        let sidekick = self
            .sessions
            .get(&top_level)
            .and_then(|record| record.summary.session.sidekick());
        let acting = self
            .sidekick_acts
            .all()
            .filter(|(_, acted_on)| acted_on.origin == Outlook::Local)
            .filter(|(_, acted_on)| {
                acted_on.session_id == top_level
                    || self.top_level_of(acted_on.session_id) == Some(top_level)
            })
            .map(|(sidekick, _)| sidekick);
        for tree in sidekick.into_iter().chain(acting) {
            if !trees.contains(&tree) {
                trees.push(tree);
            }
        }
        trees
    }

    /// Each of `roots` and every Session beneath it, at any depth.
    fn subtrees_of(&self, roots: &[SessionId]) -> Vec<SessionId> {
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        for (id, record) in &self.sessions {
            if let Some(parent) = record.snapshot.session.parent {
                children.entry(parent).or_default().push(*id);
            }
        }
        let mut visited = roots.iter().copied().collect::<HashSet<_>>();
        let mut walk = roots.to_vec();
        let mut at = 0;
        while at < walk.len() {
            for child in children.get(&walk[at]).into_iter().flatten() {
                if visited.insert(*child) {
                    walk.push(*child);
                }
            }
            at += 1;
        }
        walk
    }

    /// The rows recording the Subagents `spawner` spawned, in the order its
    /// Transcript holds them: read from its history where that is held, and
    /// otherwise from the rows read alone for its tree, or none where they
    /// were not.
    pub(super) fn subagent_rows_of(&self, spawner: SessionId) -> Vec<&Activity> {
        if self.is_deferred(spawner) {
            return self
                .stored_subagent_rows
                .get(&spawner)
                .into_iter()
                .flatten()
                .collect();
        }
        self.sessions
            .get(&spawner)
            .into_iter()
            .flat_map(|record| &record.snapshot.activities)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use diesel::{Connection, SqliteConnection, connection::SimpleConnection};

    use super::super::restoration_tests::persisted;
    use super::*;
    use crate::storage::{StorageRepository, StorageWriter};

    fn act(sidekick: SessionId, session_id: SessionId, acted_at: u64) -> StoredSidekickAct {
        StoredSidekickAct {
            sidekick,
            origin: Outlook::Local,
            session_id,
            acted_at: SessionTimestamp(acted_at),
            began: false,
            resolved: true,
            confirmed: true,
            pairing: String::new(),
            beginning: None,
            evidence: ActEvidence::SessionStanding.stored(),
        }
    }

    fn database(directory: &std::path::Path) -> SqliteConnection {
        SqliteConnection::establish(
            directory
                .join("suru.db")
                .to_str()
                .expect("the fixture's path is UTF-8"),
        )
        .expect("open the database")
    }

    #[tokio::test]
    async fn an_act_riding_a_change_lands_with_it_after_the_sidekicks_session_it_names() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open storage");
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        let sidekick = persisted(directory.path(), None);
        let subsession = persisted(directory.path(), None);
        let begun = act(
            sidekick.snapshot.session.id,
            subsession.snapshot.session.id,
            3,
        );
        // Neither Session has landed yet when the act riding the second one's
        // creation is flushed.
        sink.created(sidekick, Vec::new());
        sink.created(subsession, vec![begun.clone()]);
        writer.shutdown().await.expect("stop the writer");

        assert_eq!(
            repository.sidekick_acts().await.expect("read the acts"),
            [begun]
        );
    }

    #[tokio::test]
    async fn an_act_whose_write_fails_is_kept_and_written_once_storage_takes_it() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open storage");
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        let sidekick = persisted(directory.path(), None);
        let target = persisted(directory.path(), None);
        let interrupted = act(sidekick.snapshot.session.id, target.snapshot.session.id, 3);
        let (session, revision) = (target.snapshot.session.clone(), target.snapshot.revision);
        sink.created(sidekick, Vec::new());
        sink.created(target, Vec::new());

        // Storage refuses the act: the table it is written to is gone.
        database(directory.path())
            .batch_execute("ALTER TABLE sidekick_acts RENAME TO sidekick_acts_away;")
            .expect("take the table away");
        sink.record_sidekick_act(interrupted.clone());
        // Commands are taken in order, so once this one is answered the act
        // has been tried, and failed.
        sink.location_changed(session, revision)
            .expect("the writer goes on after the failure");
        database(directory.path())
            .batch_execute("ALTER TABLE sidekick_acts_away RENAME TO sidekick_acts;")
            .expect("give the table back");

        writer.shutdown().await.expect("stop the writer");
        assert_eq!(
            repository.sidekick_acts().await.expect("read the acts"),
            [interrupted],
            "the act was kept, and written once storage took it"
        );
    }

    /// The second review's item 7: an act on a Remote's Session not yet
    /// confirmed that began no Session, held before what it left there was
    /// kept, left something or nothing no one can now tell — so bringing the
    /// database up to date lets it go, rather than leave its Session merely
    /// standing to confirm it. A beginning not yet confirmed, which named
    /// its Session beforehand, is kept with its Session standing all it
    /// needs, and a confirmed act is kept as it was.
    #[tokio::test]
    async fn an_unconfirmed_act_held_before_what_it_left_was_kept_is_let_go_by_the_upgrade() {
        let directory = tempfile::tempdir().expect("create a data root");
        drop(
            StorageRepository::open(directory.path())
                .await
                .expect("open storage"),
        );
        let (sidekick, prompted, begun, done) = (
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
        );
        let mut connection = database(directory.path());
        connection
            .batch_execute(&format!(
                "PRAGMA foreign_keys = OFF; ALTER TABLE sidekick_acts DROP COLUMN evidence; \
                 DELETE FROM __diesel_schema_migrations WHERE version = '20261007048200'; \
                 INSERT INTO sidekick_acts (sidekick_session_id, origin, session_id, acted_at, \
                 began, resolved, confirmed, pairing, beginning) VALUES \
                 ('{sidekick}', 'studio', '{prompted}', 1, 0, 0, 0, 'SHA256:studio', NULL), \
                 ('{sidekick}', 'studio', '{begun}', 2, 1, 1, 0, 'SHA256:studio', NULL), \
                 ('{sidekick}', 'studio', '{done}', 3, 0, 1, 1, 'SHA256:studio', NULL);"
            ))
            .expect("hold acts as the schema before what an act left was kept");
        drop(connection);

        let repository = StorageRepository::open(directory.path())
            .await
            .expect("bring storage up to date");
        let mut kept = repository
            .sidekick_acts()
            .await
            .expect("read the acts")
            .into_iter()
            .map(|act| (act.session_id, act.confirmed, act.evidence))
            .collect::<Vec<_>>();
        kept.sort_by_key(|(session_id, ..)| *session_id == done);
        assert_eq!(
            kept,
            [
                (begun, false, "\"session_standing\"".to_owned()),
                (done, true, "\"session_standing\"".to_owned()),
            ]
        );
    }

    /// Two acts on one Remote Session, neither yet confirmed, ask what both
    /// ask: anything either left, or — where either leaves nothing behind —
    /// the Session standing, which confirms the Sidekick's hand in it.
    #[test]
    fn acts_not_yet_confirmed_ask_together_the_least_either_asks() {
        let (sent, more) = (
            super::super::RemoteContribution::Prompt(crate::protocol::PromptId::new()),
            super::super::RemoteContribution::Prompt(crate::protocol::PromptId::new()),
        );
        let unconfirmed = |evidence| Act {
            confirmed: false,
            resolved: false,
            evidence,
            ..Act::here(SessionTimestamp(1))
        };
        let mut act = unconfirmed(ActEvidence::Left(vec![sent]));
        act.take_up(unconfirmed(ActEvidence::Left(vec![more, sent])));
        assert_eq!(act.evidence, ActEvidence::Left(vec![sent, more]));
        act.take_up(unconfirmed(ActEvidence::SessionStanding));
        assert_eq!(act.evidence, ActEvidence::SessionStanding);
        assert_eq!(
            ActEvidence::from_stored(&ActEvidence::Left(vec![sent]).stored()),
            ActEvidence::Left(vec![sent])
        );
        assert_eq!(
            ActEvidence::from_stored("[]"),
            ActEvidence::Left(Vec::new()),
            "evidence no longer read is unavailable, so nothing confirms it"
        );
    }
}
