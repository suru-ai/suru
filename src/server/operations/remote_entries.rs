//! Keeping a Remote in view for the trees listing its Sessions.
//!
//! A Sidekick's tree lists the Sessions it began or acted on at a Remote, and
//! what each is doing is the Remote's to say (see
//! [`crate::sessions::SessionStore::remote_read`]). While some Client watches
//! such a tree, this Server keeps the Remote in view as a Client keeping a
//! Remote in view under Everywhere does: it follows the Remote's catalog of
//! Sessions through the Pairing, reads the Remote's listing whenever it begins
//! following it or loses its place in it, and takes up every change after.
//! Beside its catalog, it follows the tree each Session listed there heads on
//! the Remote, so the Subagents beneath it stand beneath it here and its own
//! work is read as a Session's of this Server is; each is followed only
//! while the Remote is, under the same Pairing.
//! A Remote that does not answer — or stops answering partway — is held as
//! silent, so its Sessions stand in the tree with nothing stale beside them,
//! and is tried again after the retry interval for as long as a tree lists
//! it. A Remote whose Pairing has ended takes its Sessions out of the tree
//! with it. Once no Client watches a tree listing the Remote, it is let go of,
//! and read afresh when one does again.
//!
//! A Remote is kept in view the same way, whatever any Client is showing,
//! while a Sidekick here is owed a Sidekick Report of one of its Sessions —
//! and no longer than that (see [`crate::sessions::SessionStore`]'s Remote
//! Reports). Each tree acted in there is read afresh — its outline, which the
//! Remote gives in one moment — whenever following begins or loses its
//! place, whenever the Remote's catalog or the tree's own stream says
//! something moved in it, and, for a tree no stream of its own is followed
//! for, each poll interval; never more often than the read interval allows,
//! what moves meanwhile read with it. The trees heading what is owed are
//! followed ahead of the rest, taking the place of one only a Client
//! watches. One read failing — a tree's outline, a Turn's final Message, the
//! listing — while the catalog answers is no outage: what is owed is kept,
//! and what failed is read again each poll interval. A tree that has grown
//! past what this Server reads of a Remote at once has each Sidekick owed
//! Reports in it told so once, and a Turn settled in a Session too large to
//! read is told without its final Message. A Remote is held as not
//! answering only where its catalog stops — ends, cannot be opened, or says
//! nothing for the silence limit — and it then does not answer when asked
//! whether it answers. A Remote that stops answering so, or whose Pairing
//! ends, while Reports are owed from it through that Pairing tells each
//! Sidekick owed them so, once.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use tokio::{sync::Notify, task::JoinHandle};

use super::{
    OriginRefusal, SessionOperations,
    origins::{Paired, RemoteReadFailure, SESSIONS_PATH},
};
use crate::protocol::{
    SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT, SUBAGENT_TREE_SNAPSHOT_EVENT,
    SUBAGENT_TREE_UPDATED_EVENT, SessionCatalogChange, SessionCatalogSnapshot,
    SessionCatalogUpdate, SessionId, SessionTreeOutline, SnapshotWithSummary, SubagentTreeChange,
    SubagentTreeSnapshot, SubagentTreeUpdate,
};
use crate::provider::SidekickOriginLoss;
use crate::server::RemoteWatchLimits;
use crate::sessions::{
    Pairing, RemoteRaise, TreeBounds, settled_report, settled_report_past_budget,
};

/// Where the Session API streams its catalog of Sessions.
const CATALOG_EVENTS_PATH: &str = "/v1/session-events";

/// The Remotes this Server keeps in view, by name, each with what asks more
/// of following it.
#[derive(Clone)]
pub(crate) struct RemoteWatches {
    running: Arc<Mutex<HashMap<String, Arc<Asked>>>>,
    /// How long a Remote that does not answer waits before it is tried
    /// again.
    retry: Duration,
    /// How often a Remote kept in view looks for whether any Client still
    /// watches a tree listing it, and whether its Pairing still stands as it
    /// did.
    check: Duration,
    /// How long a Remote kept in view may say nothing at all — not even the
    /// keep-alive its stream sends — before it is held as not answering.
    silence_limit: Duration,
    /// How much keeping Remotes in view takes on.
    limits: RemoteWatchLimits,
    /// Where the next reading of a Remote begins asking after the Sessions
    /// acted on whose heading is not yet known, so each is asked in turn
    /// however many there are.
    resolution_turn: Arc<AtomicUsize>,
    /// How many trees of Remotes' Sessions are followed now, across every
    /// Remote.
    trees_followed: Arc<AtomicUsize>,
    /// How often, at most, the trees of a Remote owed Reports are read
    /// again, however much moves in them meanwhile.
    report_read_interval: Duration,
    /// How often a tree owed Reports that no stream of its own is followed
    /// for is read again, whether or not the Remote said anything moved.
    report_poll_interval: Duration,
}

impl RemoteWatches {
    pub(crate) fn new(
        retry: Duration,
        check: Duration,
        silence_limit: Duration,
        limits: RemoteWatchLimits,
    ) -> Self {
        Self {
            running: Arc::default(),
            retry,
            check,
            silence_limit,
            limits,
            resolution_turn: Arc::default(),
            trees_followed: Arc::default(),
            report_read_interval: Duration::ZERO,
            report_poll_interval: Duration::from_secs(5),
        }
    }

    /// Reads the trees of a Remote owed Reports no more often than once each
    /// `read`, and a tree no stream of its own is followed for once each
    /// `poll` whatever the Remote says.
    pub(crate) fn with_report_reads(mut self, read: Duration, poll: Duration) -> Self {
        self.report_read_interval = read;
        self.report_poll_interval = poll;
        self
    }
}

/// What asks more of following a Remote than taking up what it says.
#[derive(Default)]
struct Asked {
    /// Read its listing again.
    reread: Notify,
    /// Read again the Sessions owed Reports that something moved in.
    stirred: Notify,
}

/// How following a Remote ended.
enum Followed {
    /// No Client watches a tree listing it any more.
    Unwatched,
    /// Its catalog stopped, or could not be opened: it did not answer, or
    /// stopped answering — where it does not answer when asked after.
    Silent,
    /// Its catalog stopped, or could not be opened, though it answers when
    /// asked after: followed afresh after the retry interval.
    Faltered,
    /// Its Pairing has ended.
    Unpaired,
    /// Its name was paired anew, to another key, while it was followed, so
    /// nothing it said under the Pairing before is its to say any more.
    Repaired,
}

/// How following the tree a Remote's Session heads there ended.
enum TreeFollowed {
    /// It lost its place among the changes, so it is read afresh at once.
    Behind,
    /// The Remote did not say it, or stopped saying it.
    Silent,
}

/// The trees of a Remote's Sessions followed while the Remote is, each by
/// the Session heading it there.
#[derive(Default)]
struct TreeFollows {
    running: HashMap<SessionId, JoinHandle<()>>,
}

/// One of the trees of Remotes' Sessions this Server follows at once, given
/// back as the following of it ends, however it ends.
struct TreeSlot(Arc<AtomicUsize>);

impl TreeSlot {
    /// A slot among the `limit` this Server follows at once, counted by
    /// `followed`, where one is free.
    fn take(followed: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        followed
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                (taken < limit).then_some(taken + 1)
            })
            .ok()
            .map(|_| Self(followed.clone()))
    }
}

impl Drop for TreeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl From<OriginRefusal> for Followed {
    fn from(refusal: OriginRefusal) -> Self {
        match refusal {
            OriginRefusal::UnknownRemote(_) => Self::Unpaired,
            OriginRefusal::Silent(silent) if silent.is_repaired() => Self::Repaired,
            OriginRefusal::Silent(_) => Self::Silent,
        }
    }
}

impl SessionOperations {
    /// Keeps in view every Remote holding a Session the tree `session_id`
    /// belongs to lists, once some Client watches that tree: each such
    /// Session joins the tree as soon as its Remote is read.
    pub(crate) fn keep_remotes_in_view(&self, session_id: SessionId) {
        // A Remote already kept in view reads afresh too, so the trees its
        // Sessions head there are followed for this tree at once.
        for remote in self.sessions.remotes_listed_with(session_id) {
            self.keep_remote_in_view(&remote, true);
        }
    }

    /// Keeps the Remote `remote` in view where some Client watches a tree
    /// listing it, or a Sidekick is owed Reports there, reading its listing
    /// again where `reread` — a Session newly acted on there must be read
    /// before it can stand in a tree, or be followed for what is owed of it.
    pub(super) fn keep_remote_in_view(&self, remote: &str, reread: bool) {
        let mut running = self
            .remote_watches
            .running
            .lock()
            .expect("Remote watch lock is not poisoned");
        if let Some(asked) = running.get(remote) {
            if reread {
                asked.reread.notify_one();
            }
            return;
        }
        if !self.sessions.is_remote_watched(remote) {
            return;
        }
        let asked = Arc::new(Asked::default());
        running.insert(remote.to_owned(), asked.clone());
        tokio::spawn(self.clone().keep_in_view(remote.to_owned(), asked));
    }

    /// Follows the Remote `remote` for as long as some Client watches a tree
    /// listing it, or a Sidekick is owed Reports there, trying it again after
    /// the retry interval while it does not answer. A Remote that stops
    /// answering, or is paired no longer, tells each Sidekick owed Reports
    /// there so, once, and owes them nothing more.
    async fn keep_in_view(self, remote: String, asked: Arc<Asked>) {
        loop {
            // Only what was owed through the Pairing that was followed — or,
            // where none stood, through any made before it was looked for —
            // is lost with it: what was owed through a Pairing made anew by
            // the name meanwhile, even to the same key, is the next
            // following's.
            let (followed, ended) = self.follow(&remote, &asked).await;
            match followed {
                // What it said under its old Pairing goes with that Pairing;
                // where the name is paired anew, it is followed afresh under
                // the new one.
                Followed::Unpaired | Followed::Repaired => {
                    self.sessions
                        .remote_reports_lost(&remote, ended, SidekickOriginLoss::Unpaired);
                    self.sessions.remote_unpaired(&remote);
                    if self.let_go_unless_watched(&remote) {
                        return;
                    }
                }
                Followed::Silent => {
                    self.sessions.remote_silent(&remote);
                    self.sessions.remote_reports_lost(
                        &remote,
                        ended,
                        SidekickOriginLoss::StoppedAnswering,
                    );
                    if self.let_go_unless_watched(&remote) {
                        return;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(self.remote_watches.retry) => {}
                        () = asked.reread.notified() => {}
                    }
                }
                Followed::Faltered => {
                    if self.let_go_unless_watched(&remote) {
                        return;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(self.remote_watches.retry) => {}
                        () = asked.reread.notified() => {}
                    }
                }
                Followed::Unwatched => {
                    if self.let_go_unless_watched(&remote) {
                        return;
                    }
                }
            }
        }
    }

    /// Lets go of the Remote `remote` unless some Client watches a tree
    /// listing it, or something is owed there, and a Pairing stands by its
    /// name — whichever one, so a name paired anew while the following of
    /// the one before ended goes on being followed under the new one —
    /// answering whether it did. Decided under the same lock a Client's new
    /// interest, or a Sidekick's new act, takes, so interest arriving now is
    /// never lost: it either finds the following still running, and has it
    /// read afresh, or finds none, and begins one.
    fn let_go_unless_watched(&self, remote: &str) -> bool {
        let mut running = self
            .remote_watches
            .running
            .lock()
            .expect("Remote watch lock is not poisoned");
        if self.remotes.pairing(remote).is_ok() && self.sessions.is_remote_watched(remote) {
            return false;
        }
        running.remove(remote);
        self.sessions.remote_unwatched(remote);
        true
    }

    /// Why following the Remote `paired` should stop now, where it should: no
    /// Client watches a tree listing it and nothing is owed there, or its
    /// Pairing does not stand as it did when following began.
    fn ought_to_stop(&self, paired: &Paired) -> Option<Followed> {
        if !self.sessions.is_remote_watched(&paired.remote.name) {
            return Some(Followed::Unwatched);
        }
        self.remotes
            .still_paired_by(paired)
            .err()
            .map(Followed::from)
    }

    /// Resolves once following the Remote `paired` should stop, looking each
    /// check interval and at every change to the Remotes paired, answering
    /// why.
    async fn stopping(&self, paired: &Paired) -> Followed {
        let mut check = tokio::time::interval(self.remote_watches.check);
        check.reset();
        let mut pairings = self.remotes.pairing_changes();
        loop {
            tokio::select! {
                _ = check.tick() => {}
                _ = pairings.changed() => {}
            }
            if let Some(stop) = self.ought_to_stop(paired) {
                return stop;
            }
        }
    }

    /// Follows the Remote `remote`'s catalog of Sessions under the Pairing
    /// standing as it begins, reading its listing when it begins, when
    /// asked, and whenever it loses its place in the catalog or the Remote
    /// says what it holds is to be read again — until no Client watches a
    /// tree listing it, its Pairing changes, or it stops answering, or says
    /// nothing at all for the silence limit. Opening the catalog gives way as
    /// soon as following should stop. The trees its Sessions head there are
    /// followed beside it, and let go of with it.
    ///
    /// Everything following does — opening the catalog, reading the listing
    /// again, asking which Session heads each one acted on — gives way the
    /// moment following should stop, so nothing of it outlives the Pairing
    /// it began under.
    ///
    /// Answers how it ended, and the generation of the Pairing it followed
    /// — or, where none stood by that name, of the last made before it was
    /// looked for — every Pairing by the name up to which has ended.
    async fn follow(&self, remote: &str, asked: &Arc<Asked>) -> (Followed, u64) {
        let made = self.remotes.pairings_made();
        let paired = match self.remotes.pairing(remote) {
            Ok(paired) => paired,
            Err(refusal) => return (refusal.into(), made),
        };
        let mut trees = TreeFollows::default();
        let followed = tokio::select! {
            followed = self.follow_with(&paired, asked, &mut trees) => followed,
            stop = self.stopping(&paired) => stop,
        };
        for (_, following) in trees.running.drain() {
            following.abort();
            let _ = following.await;
        }
        // Its catalog stopping is not enough to hold it as not answering:
        // only where it then does not answer when asked after is it so held.
        let followed = match followed {
            Followed::Silent => match self.remotes.answers(remote).await {
                Ok(()) => Followed::Faltered,
                Err(refusal) => refusal.into(),
            },
            followed => followed,
        };
        (followed, paired.generation)
    }

    /// [`Self::follow`] under the Pairing `paired`, following the trees its
    /// Sessions head there in `trees`.
    async fn follow_with(
        &self,
        paired: &Paired,
        asked: &Arc<Asked>,
        trees: &mut TreeFollows,
    ) -> Followed {
        let remote = paired.remote.name.as_str();
        let opening = async {
            let mut events = self
                .remotes
                .events(
                    remote,
                    CATALOG_EVENTS_PATH,
                    self.remote_watches.silence_limit,
                )
                .await?;
            let opened = tokio::time::timeout(self.remotes.timeout(), events.next()).await;
            let Ok(Some(Ok(opened))) = opened else {
                return Ok(None);
            };
            let revision = (opened.event == SESSION_CATALOG_SNAPSHOT_EVENT)
                .then(|| serde_json::from_str::<SessionCatalogSnapshot>(&opened.data).ok())
                .flatten()
                .map(|snapshot| snapshot.revision);
            Ok::<_, OriginRefusal>(revision.map(|revision| (events, revision)))
        };
        let (mut events, mut revision) = match opening.await {
            Ok(Some(opened)) => opened,
            Ok(None) => return Followed::Silent,
            Err(refusal) => return refusal.into(),
        };
        if let Some(stop) = self.ought_to_stop(paired) {
            return stop;
        }
        // A listing that could not be read is read again each poll interval.
        let mut reread_due = false;
        if let Err(refusal) = self.read_remote(remote).await {
            if let Some(ended) = ended_by(refusal) {
                return ended;
            }
            reread_due = true;
        }
        if let Err(refusal) = self.follow_owed_reports(paired, true).await {
            return refusal.into();
        }
        let mut check = tokio::time::interval(self.remote_watches.check);
        check.reset();
        let mut poll = tokio::time::interval(self.remote_watches.report_poll_interval);
        poll.reset();
        let mut pairings = self.remotes.pairing_changes();
        let mut last_read = tokio::time::Instant::now();
        loop {
            // Nothing more owed there, and no Client watching, lets it go at
            // once.
            if let Some(stop) = self.ought_to_stop(paired) {
                return stop;
            }
            self.follow_remote_trees(paired, trees, asked).await;
            tokio::select! {
                _ = pairings.changed() => {
                    if let Some(stop) = self.ought_to_stop(paired) {
                        return stop;
                    }
                }
                event = events.next() => {
                    let Some(Ok(event)) = event else {
                        return Followed::Silent;
                    };
                    // Nothing a Pairing no longer standing said is taken up.
                    if let Some(stop) = self.ought_to_stop(paired) {
                        return stop;
                    }
                    if event.event != SESSION_CATALOG_UPDATED_EVENT {
                        continue;
                    }
                    let Ok(update) = serde_json::from_str::<SessionCatalogUpdate>(&event.data)
                    else {
                        return Followed::Silent;
                    };
                    let in_place = update.revision.immediately_follows(revision);
                    revision = update.revision;
                    // A Session begun there may be one a Sidekick here is
                    // about to name, a change missed leaves the reading
                    // behind, and the Remote may say what it holds is to be
                    // read again, so each reads the listing again.
                    let reread = !in_place
                        || matches!(
                            update.change,
                            SessionCatalogChange::Created { .. }
                                | SessionCatalogChange::Invalidated { .. }
                        );
                    if reread {
                        if let Err(refusal) = self.read_remote(remote).await {
                            if let Some(ended) = ended_by(refusal) {
                                return ended;
                            }
                            reread_due = true;
                        }
                        // Every tree owed Reports is read again too, with
                        // what else moves meanwhile.
                        if self.sessions.stir_all_remote_reports(remote) {
                            asked.stirred.notify_one();
                        }
                    } else {
                        // What moved is read for what is owed in it with what
                        // else moves meanwhile, no more often than the read
                        // interval allows.
                        if stirring(&update.change)
                            .is_some_and(|head| self.sessions.stir_remote_reports(remote, head))
                        {
                            asked.stirred.notify_one();
                        }
                        self.sessions.remote_changed(remote, update.change);
                    }
                }
                () = asked.reread.notified() => {
                    if let Err(refusal) = self.read_remote(remote).await {
                        if let Some(ended) = ended_by(refusal) {
                            return ended;
                        }
                        reread_due = true;
                    }
                    if self.sessions.stir_all_remote_reports(remote) {
                        asked.stirred.notify_one();
                    }
                }
                () = asked.stirred.notified() => {
                    tokio::time::sleep_until(last_read + self.remote_watches.report_read_interval)
                        .await;
                    if let Err(refusal) = self.follow_owed_reports(paired, false).await {
                        return refusal.into();
                    }
                    last_read = tokio::time::Instant::now();
                }
                // A tree owed Reports that no stream of its own is followed
                // for is read all the same, so it is never left unheard; and
                // what failed to be read is read again.
                _ = poll.tick() => {
                    let unfollowed = self
                        .sessions
                        .remote_trees_wanted(remote)
                        .1
                        .into_iter()
                        .filter(|head| !trees.running.contains_key(head))
                        .filter(|head| self.sessions.stir_remote_reports(remote, *head))
                        .count();
                    if self.sessions.stir_unread_remote_reports(remote) || unfollowed > 0 {
                        asked.stirred.notify_one();
                    }
                    if std::mem::take(&mut reread_due) {
                        asked.reread.notify_one();
                    }
                }
                _ = check.tick() => {
                    if let Some(stop) = self.ought_to_stop(paired) {
                        return stop;
                    }
                }
            }
        }
    }

    /// Follows the tree each Session of the Remote `remote` that a watched
    /// tree lists — or that heads what a Sidekick is owed Reports of there —
    /// heads there, where it is not followed yet — or following it ended —
    /// and lets go of each no longer wanted, forgetting it.
    async fn follow_remote_trees(
        &self,
        paired: &Paired,
        trees: &mut TreeFollows,
        asked: &Arc<Asked>,
    ) {
        let remote = paired.remote.name.as_str();
        let (wanted, owed) = self.sessions.remote_trees_wanted(remote);
        let unwanted = trees
            .running
            .keys()
            .filter(|session_id| !wanted.contains(session_id))
            .copied()
            .collect::<Vec<_>>();
        for session_id in unwanted {
            if let Some(following) = trees.running.remove(&session_id) {
                following.abort();
                let _ = following.await;
            }
            self.sessions.remote_tree_dropped(remote, session_id);
        }
        trees
            .running
            .retain(|_, following| !following.is_finished());
        // Within the trees followed of one Remote, and of every Remote at
        // once: those past them stand without their Subagents, saying so.
        // Those heading what is owed are followed first, so what is owed
        // there is heard of as it moves.
        let mut wanted = wanted.into_iter().collect::<Vec<_>>();
        wanted.sort_by_key(|session_id| (!owed.contains(session_id), session_id.as_uuid()));
        let mut unfollowed = std::collections::HashSet::new();
        for session_id in wanted {
            if trees.running.contains_key(&session_id) {
                continue;
            }
            let mut slot = self.tree_slot(trees);
            // A tree heading what is owed takes the place of one only a
            // Client watches.
            if slot.is_none()
                && owed.contains(&session_id)
                && let Some(yielding) = trees
                    .running
                    .keys()
                    .find(|followed| !owed.contains(followed))
                    .copied()
            {
                if let Some(following) = trees.running.remove(&yielding) {
                    following.abort();
                    let _ = following.await;
                }
                self.sessions.remote_tree_dropped(remote, yielding);
                unfollowed.insert(yielding);
                slot = self.tree_slot(trees);
            }
            let Some(slot) = slot else {
                unfollowed.insert(session_id);
                continue;
            };
            trees.running.insert(
                session_id,
                tokio::spawn(self.clone().follow_remote_tree(
                    paired.clone(),
                    session_id,
                    asked.clone(),
                    slot,
                )),
            );
        }
        self.sessions.remote_trees_unfollowed(remote, unfollowed);
    }

    /// A slot among the trees followed of the Remote `trees` follows, and
    /// among those of every Remote, where one is free.
    fn tree_slot(&self, trees: &TreeFollows) -> Option<TreeSlot> {
        (trees.running.len() < self.remote_watches.limits.trees_per_remote)
            .then(|| {
                TreeSlot::take(
                    &self.remote_watches.trees_followed,
                    self.remote_watches.limits.trees_overall,
                )
            })
            .flatten()
    }

    /// How much of one Remote Session's tree is kept.
    fn tree_bounds(&self) -> TreeBounds {
        TreeBounds {
            entries: self.remote_watches.limits.tree_entries,
            depth: self.remote_watches.limits.tree_depth,
        }
    }

    /// Follows the tree the Session `session_id` of the Remote `remote`
    /// heads there until it is let go of or the Remote says it is deleted:
    /// read afresh at once whenever it loses its place among the changes,
    /// and — forgotten — tried again after the retry interval while the
    /// Remote does not say it. Whatever moves in it has what is owed there
    /// read again, through `asked`.
    async fn follow_remote_tree(
        self,
        paired: Paired,
        session_id: SessionId,
        asked: Arc<Asked>,
        _slot: TreeSlot,
    ) {
        let path = format!("/v1/sessions/{session_id}/subagent-tree");
        let remote = paired.remote.name.clone();
        loop {
            match self
                .follow_remote_tree_once(&paired, session_id, &path, &asked)
                .await
            {
                Some(TreeFollowed::Behind) => continue,
                Some(TreeFollowed::Silent) => {
                    self.sessions.remote_tree_silent(&remote, session_id);
                    tokio::time::sleep(self.remote_watches.retry).await;
                }
                None => {
                    self.sessions.remote_tree_dropped(&remote, session_id);
                    return;
                }
            }
        }
    }

    /// Reads the tree the Session `session_id` of the Remote `remote` heads
    /// there, through `path`, and takes up each change to it after,
    /// answering how that ended — `None` where the Remote said it is
    /// deleted, with all beneath it.
    async fn follow_remote_tree_once(
        &self,
        paired: &Paired,
        session_id: SessionId,
        path: &str,
        asked: &Asked,
    ) -> Option<TreeFollowed> {
        let remote = paired.remote.name.as_str();
        let Ok(mut events) = self
            .remotes
            .events(remote, path, self.remote_watches.silence_limit)
            .await
        else {
            return Some(TreeFollowed::Silent);
        };
        let mut revision = None;
        while let Some(Ok(event)) = events.next().await {
            // Nothing a Pairing no longer standing said is taken up.
            if self.remotes.still_paired_by(paired).is_err() {
                return None;
            }
            if event.event == SUBAGENT_TREE_SNAPSHOT_EVENT {
                let Ok(tree) = serde_json::from_str::<SubagentTreeSnapshot>(&event.data) else {
                    return Some(TreeFollowed::Silent);
                };
                revision = Some(tree.revision);
                self.sessions
                    .remote_tree_read(remote, session_id, tree, self.tree_bounds());
            } else if event.event == SUBAGENT_TREE_UPDATED_EVENT {
                let Ok(update) = serde_json::from_str::<SubagentTreeUpdate>(&event.data) else {
                    return Some(TreeFollowed::Silent);
                };
                // A change missed leaves the reading behind.
                if !revision.is_some_and(|revision| update.revision.immediately_follows(revision)) {
                    return Some(TreeFollowed::Behind);
                }
                revision = Some(update.revision);
                if update.change == SubagentTreeChange::TreeDeleted {
                    return None;
                }
                self.sessions.remote_tree_changed(
                    remote,
                    session_id,
                    update.change,
                    self.tree_bounds(),
                );
            } else {
                continue;
            }
            if self.sessions.stir_remote_reports(remote, session_id) {
                asked.stirred.notify_one();
            }
        }
        Some(TreeFollowed::Silent)
    }

    /// The top-level Session of the Remote `remote` heading the Subagents
    /// `session_id` stands among there — itself, where it is no Subagent's —
    /// as the Remote's own tree says, so an act on a Subagent's Session
    /// stands beneath a Sidekick by the Session heading it, as one on this
    /// Server's does. `None` where the Remote does not say.
    pub(super) async fn remote_top_level(
        &self,
        remote: &str,
        session_id: SessionId,
    ) -> Option<SessionId> {
        let mut events = self
            .remotes
            .events(
                remote,
                &format!("/v1/sessions/{session_id}/subagent-tree"),
                self.remotes.timeout(),
            )
            .await
            .ok()?;
        let opened = tokio::time::timeout(self.remotes.timeout(), events.next())
            .await
            .ok()??
            .ok()?;
        let tree = (opened.event == SUBAGENT_TREE_SNAPSHOT_EVENT)
            .then(|| serde_json::from_str::<SubagentTreeSnapshot>(&opened.data).ok())??;
        Some(top_level_in(&tree, session_id))
    }

    /// Reads afresh the outline of each tree of the Remote `paired` a
    /// Sidekick is owed Reports of — every one, where `all`, and otherwise
    /// those something moved in — takes up what each says, and tells each
    /// Sidekick what it finds owed: a Turn's settling once the Session it
    /// settled in is read for what its Agent wrote. A tree the Remote does
    /// not hold, or cannot read, has nothing more to follow. One whose read
    /// failed is no outage: what is owed in it is kept, and it is read again
    /// each poll interval — and where it has grown past what this Server
    /// reads of a Remote at once, each Sidekick owed Reports in it is told
    /// so, once. A Turn's settling in a Session too large to read is told
    /// without its final Message; one whose Session failed to be read is
    /// told once a later read of its tree reads it. Refused only where the
    /// Pairing it was read through no longer stands.
    async fn follow_owed_reports(&self, paired: &Paired, all: bool) -> Result<(), OriginRefusal> {
        let remote = paired.remote.name.as_str();
        let pairing = paired.pairing();
        let Some(own) = self.remotes.own_fingerprint() else {
            return Ok(());
        };
        let reading = self.sessions.remote_reports_to_read(remote, all);
        for read_by in reading.trees {
            let asked_at = self.sessions.moment();
            let outline = self
                .remotes
                .get::<SessionTreeOutline>(remote, &format!("{SESSIONS_PATH}/{read_by}/outline"))
                .await;
            // Nothing a Pairing no longer standing said is taken up.
            self.remotes.still_paired_by(paired)?;
            let outline = match outline {
                Ok(outline) => outline,
                Err(RemoteReadFailure::SessionNotFound | RemoteReadFailure::SessionUnreadable) => {
                    self.sessions
                        .let_go_of_remote_reports(remote, read_by, reading.covered);
                    continue;
                }
                Err(RemoteReadFailure::Origin(refusal)) => {
                    self.sessions.remote_tree_unread(
                        remote,
                        &pairing,
                        read_by,
                        refusal.is_past_budget(),
                    );
                    continue;
                }
            };
            // What it shows of acts not yet confirmed judges them.
            let confirmed = self.sessions.judge_remote_acts(
                remote,
                &pairing,
                Some(&own),
                &outline.sessions,
                true,
                asked_at,
            );
            self.stand_confirmed_beginnings(remote, confirmed);
            let following = self.sessions.follow_remote_outline(
                remote,
                &pairing,
                &own,
                reading.covered,
                read_by,
                &outline,
            );
            let mut told = Vec::new();
            let mut withheld = false;
            for raise in following.raises {
                match raise {
                    RemoteRaise::Report {
                        sidekick,
                        report,
                        told: owed,
                    } => told.push((sidekick, report, owed)),
                    RemoteRaise::Settled {
                        sidekick,
                        session_id,
                        turn_id,
                        subject,
                        owed,
                    } => {
                        // What its Agent wrote is read from the Session
                        // itself, as a Sidekick's reading of it is.
                        let read = self
                            .remotes
                            .get::<SnapshotWithSummary>(
                                remote,
                                &format!("{SESSIONS_PATH}/{session_id}/with-summary"),
                            )
                            .await;
                        self.remotes.still_paired_by(paired)?;
                        let report = match read {
                            Ok(read) => read
                                .snapshot
                                .turns
                                .iter()
                                .find(|turn| turn.id == turn_id)
                                .and_then(|turn| settled_report(subject, &read.snapshot, turn)),
                            // Gone meanwhile: nothing is left to tell of it.
                            Err(
                                RemoteReadFailure::SessionNotFound
                                | RemoteReadFailure::SessionUnreadable,
                            ) => continue,
                            // Too large to read, it is told as the outline
                            // gives it, without its final Message.
                            Err(RemoteReadFailure::Origin(refusal)) if refusal.is_past_budget() => {
                                outline
                                    .sessions
                                    .iter()
                                    .find(|snapshot| snapshot.session.id == session_id)
                                    .and_then(|snapshot| {
                                        snapshot.turns.iter().find(|turn| turn.id == turn_id)
                                    })
                                    .and_then(|turn| settled_report_past_budget(subject, turn))
                            }
                            // Not read now, it is told once it is.
                            Err(RemoteReadFailure::Origin(_)) => {
                                withheld = true;
                                continue;
                            }
                        };
                        if let Some(report) = report {
                            told.push((sidekick, report, vec![owed]));
                        }
                    }
                }
            }
            // Nothing is let go of while something found owed waits on a
            // read: the tree is read again, and finds it again.
            let spent = if withheld {
                self.sessions
                    .remote_tree_unread(remote, &pairing, read_by, false);
                Vec::new()
            } else {
                following.spent
            };
            self.sessions
                .tell_remote_reports(remote, &pairing, told, &spent);
        }
        Ok(())
    }

    /// Reads what the Remote `remote` holds now into every tree listing it,
    /// first asking it which Session heads each Session acted on there that
    /// it has not yet said of — and where it says of none, whether it holds
    /// that Session at all, an act on one it holds no longer being dropped.
    async fn read_remote(&self, remote: &str) -> Result<(), OriginRefusal> {
        let pairing = self.pairing_of(remote);
        // Asked after a bounded few at a time, each reading taking the next
        // few in turn, and never for longer than one exchange is given.
        let unresolved = self.sessions.unresolved_remote_acts(remote);
        let asked = self.remote_watches.limits.resolutions_per_read;
        let turn = self
            .remote_watches
            .resolution_turn
            .fetch_add(asked, Ordering::Relaxed);
        let asking = in_turn(&unresolved, turn, asked);
        let _ =
            tokio::time::timeout(self.remotes.timeout(), self.resolve_remote(remote, asking)).await;
        // Acts not yet confirmed that left something to be found by are
        // judged by what the trees they were made in show, a few at a time.
        let unjudged = self.sessions.unjudged_remote_acts(remote);
        let judging = in_turn(&unjudged, turn, asked);
        let _ = tokio::time::timeout(
            self.remotes.timeout(),
            self.judge_remote(remote, &pairing, judging),
        )
        .await;
        let asked_at = self.sessions.moment();
        let listed = self.remotes.sessions_of(remote).await?;
        let confirmed = self
            .sessions
            .remote_read(remote, &pairing, listed, asked_at);
        self.stand_confirmed_beginnings(remote, confirmed);
        Ok(())
    }

    /// Reads the outline of the tree each of `unjudged` belongs to on the
    /// Remote `remote`, through `pairing`, and judges the acts not yet confirmed on it by what it
    /// shows; an act on a Session the Remote holds no longer is forgotten.
    async fn judge_remote(&self, remote: &str, pairing: &Pairing, unjudged: Vec<SessionId>) {
        let own = self.remotes.own_fingerprint();
        for session_id in unjudged {
            let asked_at = self.sessions.moment();
            let outline = self
                .remotes
                .get::<SessionTreeOutline>(remote, &format!("{SESSIONS_PATH}/{session_id}/outline"))
                .await;
            match outline {
                Ok(outline) => {
                    let confirmed = self.sessions.judge_remote_acts(
                        remote,
                        pairing,
                        own.as_deref(),
                        &outline.sessions,
                        true,
                        asked_at,
                    );
                    self.stand_confirmed_beginnings(remote, confirmed);
                }
                Err(RemoteReadFailure::SessionNotFound) => {
                    self.sessions.forget_remote_session(remote, session_id);
                }
                Err(_) => {}
            }
        }
    }

    /// Asks the Remote `remote` which Session heads each of `unresolved`,
    /// the Sessions acted on there whose heading is not yet known — and
    /// where it says of none, whether it holds that Session at all, an act
    /// on one it holds no longer being dropped where it may be.
    async fn resolve_remote(&self, remote: &str, unresolved: Vec<(SessionId, bool)>) {
        for (session_id, droppable) in unresolved {
            match self.remote_top_level(remote, session_id).await {
                Some(top_level) => {
                    let confirmed = self
                        .sessions
                        .resolve_remote_acts(remote, session_id, top_level);
                    self.stand_confirmed_beginnings(remote, confirmed);
                }
                None if droppable => {
                    let read = self
                        .remotes
                        .get::<serde::de::IgnoredAny>(
                            remote,
                            &format!("{SESSIONS_PATH}/{session_id}/with-summary"),
                        )
                        .await;
                    if matches!(read, Err(RemoteReadFailure::SessionNotFound)) {
                        self.sessions.forget_remote_session(remote, session_id);
                    }
                }
                None => {}
            }
        }
    }
}

/// What following a Remote asks of a read of it that failed for `refusal`:
/// to end, where the Pairing it was read through no longer stands — and
/// otherwise nothing, one read failing while the catalog answers being no
/// evidence the Remote stopped answering.
fn ended_by(refusal: OriginRefusal) -> Option<Followed> {
    match Followed::from(refusal) {
        Followed::Silent => None,
        ended => Some(ended),
    }
}

/// The Session whose work `change`, a Remote's catalog said, may have moved
/// on — a Turn begun or settled, an Intervention asked or settled, its work
/// beneath it working or not — where it says of one.
fn stirring(change: &SessionCatalogChange) -> Option<SessionId> {
    match change {
        SessionCatalogChange::WorkingChanged { session_id, .. }
        | SessionCatalogChange::MonitoringChanged { session_id, .. }
        | SessionCatalogChange::StandingInputsChanged { session_id, .. }
        | SessionCatalogChange::UsageChanged { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

/// At most `asked` of `items`, beginning at the `turn`-th and going round,
/// so successive turns each ask after the next few until every one has been.
fn in_turn<T: Copy>(items: &[T], turn: usize, asked: usize) -> Vec<T> {
    if items.is_empty() {
        return Vec::new();
    }
    items
        .iter()
        .cycle()
        .skip(turn % items.len())
        .take(asked.min(items.len()))
        .copied()
        .collect()
}

/// The top-level Session heading the Subagents `session_id` stands among in
/// `tree`: itself where it heads the tree or stands beneath its Sidekick as a
/// Session of its own, and otherwise the first such Session above it, walked
/// up through the Subagents that spawned it. A Session the tree does not
/// hold is taken as its own.
fn top_level_in(tree: &SubagentTreeSnapshot, session_id: SessionId) -> SessionId {
    let heads = |at: SessionId| {
        at == tree.top_level.session_id
            || tree
                .sessions
                .iter()
                .any(|session| session.origin.is_none() && session.session_id == at)
    };
    let parents = tree
        .subagents
        .iter()
        .map(|entry| (entry.session_id, entry.parent_session_id))
        .collect::<HashMap<_, _>>();
    let mut at = session_id;
    // A Subagent cannot be its own ancestor, so a walk longer than the tree
    // is one round a cycle, and ends where it began.
    for _ in 0..=parents.len() {
        if heads(at) {
            return at;
        }
        match parents.get(&at) {
            Some(parent) => at = *parent,
            None => return session_id,
        }
    }
    session_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ActivityStatus, SessionTimestamp, SubagentTreeEntry, SubagentTreeRevision,
        SubagentTreeSession, SubagentTreeTopLevel,
    };

    fn entry(session_id: SessionId, parent_session_id: SessionId) -> SubagentTreeEntry {
        SubagentTreeEntry {
            unanswered: false,
            origin: None,
            session_id,
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

    #[test]
    fn no_more_trees_are_followed_at_once_than_there_are_slots() {
        let followed = Arc::new(AtomicUsize::new(0));
        let first = TreeSlot::take(&followed, 2).expect("a slot is free");
        let second = TreeSlot::take(&followed, 2).expect("so is another");
        assert!(TreeSlot::take(&followed, 2).is_none(), "but no third");
        drop(first);
        let third = TreeSlot::take(&followed, 2).expect("one given back is free again");
        drop((second, third));
        assert_eq!(followed.load(Ordering::Acquire), 0, "every slot given back");
    }

    #[test]
    fn a_reading_asks_after_a_bounded_few_in_turn_until_each_was_asked() {
        let unresolved = [1, 2, 3, 4, 5];
        assert_eq!(in_turn(&unresolved, 0, 2), [1, 2]);
        assert_eq!(in_turn(&unresolved, 2, 2), [3, 4]);
        assert_eq!(in_turn(&unresolved, 4, 2), [5, 1], "going round");
        assert_eq!(
            in_turn(&unresolved, 3, 9),
            [4, 5, 1, 2, 3],
            "each once at most"
        );
        assert!(in_turn::<u8>(&[], 7, 2).is_empty());
    }

    #[test]
    fn an_act_on_a_remotes_subagent_stands_by_the_session_heading_it_there() {
        let (head, subsession, child, grandchild, beneath) = (
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
            SessionId::new(),
        );
        let tree = SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                own_working_since: None,
                status: None,
                worked_ms: None,
                session_id: head,
                title: "Plan the week".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: true,
            },
            subagents: vec![
                entry(child, head),
                entry(grandchild, child),
                entry(beneath, subsession),
            ],
            sessions: vec![SubagentTreeSession {
                subagents_unshown: false,
                unconfirmed: false,
                session_id: subsession,
                origin: None,
                unanswered: false,
                title: "Fix the parser".to_owned(),
                subsession: true,
                workspace_path: std::path::PathBuf::new(),
                workspace_icon: None,
                model: None,
                status: None,
                worked_ms: None,
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                acted_at: SessionTimestamp(1),
            }],
        };
        assert_eq!(top_level_in(&tree, head), head);
        assert_eq!(
            top_level_in(&tree, grandchild),
            head,
            "walked up to its head"
        );
        assert_eq!(
            top_level_in(&tree, subsession),
            subsession,
            "a Session beneath a Sidekick there heads its own Subagents"
        );
        assert_eq!(top_level_in(&tree, beneath), subsession);
        let elsewhere = SessionId::new();
        assert_eq!(top_level_in(&tree, elsewhere), elsewhere);
    }
}
