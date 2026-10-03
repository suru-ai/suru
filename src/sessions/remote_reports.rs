//! Sidekick Reports owed of a Remote's Sessions (CONTEXT.md: Sidekick
//! Report; ADR 0044).
//!
//! A Remote knows nothing of a Peer's Sidekick: what a Sidekick carries there
//! stands as a Sidekick's on that Peer, and nothing on the Remote owes it a
//! Report. So the Sidekick's own Server holds what the Sidekick is owed
//! there, and follows it by the one rule it follows its own Sessions by (see
//! `sidekick_reports`) — the very same code, fed from what the Remote says
//! rather than from this Server's commits.
//!
//! What it holds is each act that set work going there: a Prompt sent — the
//! first of a Session begun among them — or an Answer given, by the
//! identities this Server chose for it. To learn what came of them it reads
//! the outline of the tree each was made in, which the Remote gives in one
//! moment and stamps with it (see [`crate::protocol::SessionTreeOutline`]),
//! and replays it through the rule from the acts alone, in the order the
//! Remote's one clock says it happened: each Prompt's taking by the Turn that
//! really took it, each Turn's beginning, each Answer's delivery, each
//! Intervention's asking, each Turn's settling. Read so, what happened before this Server first read the
//! tree, or while it had lost its place, is followed exactly as what it
//! hears of at once; and whatever the replay finds owed that was told before
//! is not told again. Whether each Session of the tree works is the
//! outline's to say, at that one moment, so a branch is let go of only once
//! the Remote says nothing in it works.
//!
//! Which Watches were live there at a moment already past is not among what
//! an outline says: only which are live as it is read, each Session's own,
//! and which settled waking the Agent, by the Watch Outcome each left. So a
//! Watch live now is replayed as starting when it did, and is its Sidekick's
//! by the rule; and where one no longer live is shown to have run on — a
//! Sidekick was told of it by an earlier reading, or a Watch Outcome stands
//! in a later Turn — a Continuation is taken for the Sidekick's where it
//! follows the Sidekick's Turn, or a Continuation that was its own, with no
//! Turn of anyone else's between. Where nothing shows a Watch ran on, none
//! is taken to have. That Watches ended with no such Continuation to come is
//! told only to a Sidekick told they were running, and — where they woke no
//! one — only once a reading finds it so still a while after one first did,
//! since the Continuation a Watch's settling wakes begins some time after
//! the Watch is gone.
//!
//! An act whose answer never came back owes nothing until an outline shows
//! it done — its Prompt standing there as this Peer's, its Answer delivered
//! as this act of this Peer's — and then owes what a confirmed act does; one
//! an outline read after it was held shows was never done is let go. Until
//! then the Remote is kept in view so that a read can tell. An Answer still
//! being submitted owes nothing yet.
//!
//! A Remote that stops answering while Reports are owed from it, or whose
//! Pairing ends, is told to each Sidekick owed them once, naming the
//! Sessions it was waiting on there, and everything owed there ends with it:
//! what comes of those Sessions afterwards the Sidekick learns by reading
//! them once the Remote answers again. Only what was owed through the
//! Pairing that stopped is so ended. Nothing here outlives a Server stop, and
//! nothing reaches a Transcript.
//!
//! Everything a Remote could say much of is bounded: an outline is read no
//! further than the reach budget, the Interventions told one by one in a
//! reading are so many at most, the rest counted in one Report, and the
//! Sessions a lost Remote's Report names are so many, the rest counted.
//! What is kept to tell each tree's Reports once — what was told, its Title
//! — is kept by tree, no more of it than the tree's latest replay found, and
//! forgotten with the tree's last act, however long another tree there stays
//! owed.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::protocol::{
    ActId, Activity, ActivityId, ApprovalOutcome, Author, Outlook, PromptId, PromptStatus,
    QuestionnaireId, QuestionnaireOutcome, SessionId, SessionReference, SessionSnapshot,
    SessionTimestamp, SessionTreeOutline, TurnId,
};
use crate::provider::{
    SidekickIntervention, SidekickOriginLoss, SidekickReport, SidekickReportSubject,
};

use super::{
    SessionStore, SessionStoreState,
    sidekick_reports::{
        Owed, SidekickWork, WorkTree, answer_delivered, follow_watches, intervention_asked,
        let_go_of_ended_watches, let_go_of_settled_branches, let_go_of_untaken_prompts,
        started_within, take_prompt, turn_began, turns_settled, watch_started,
    },
};

/// The most Interventions told one by one to a Sidekick of one reading of a
/// Remote's tree; any more it newly finds are counted in one Report.
const INTERVENTIONS_TOLD_AT_ONCE: usize = 16;

/// One act of a Sidekick's on a Remote's Session that it is owed Reports
/// of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteOwing {
    /// The Session it went to there: a top-level Session, or a Subagent's.
    pub(crate) session_id: SessionId,
    /// The top-level Session heading it there, where known.
    pub(crate) head: Option<SessionId>,
    /// That Session's Title, where known.
    pub(crate) title: Option<String>,
    pub(crate) contribution: RemoteContribution,
    /// Whether the Remote answered the act; otherwise it owes nothing until
    /// a read finds it done.
    pub(crate) confirmed: bool,
}

/// What a Sidekick asked of a Remote's Session that it is owed Reports of,
/// by what it leaves there to be found by.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteContribution {
    /// A Prompt it sent, the first of a Session it began among them.
    Prompt(PromptId),
    /// An Answer it gave a Questionnaire there, as the act `act`.
    Answer {
        questionnaire: QuestionnaireId,
        act: ActId,
    },
}

/// A Pairing with a Remote that something was carried through: the key
/// fingerprint it was made with, and its generation — which of the Pairings
/// made since this Server started it is, none for one it started with. A
/// name unpaired and paired again, even to the same key, is another
/// Pairing, made later.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Pairing {
    pub(crate) fingerprint: String,
    pub(crate) generation: u64,
}

/// The work Sidekicks set going in Remotes' Sessions, by the Remote's name.
#[derive(Default)]
pub(super) struct RemoteReports {
    by_remote: HashMap<String, OwedThere>,
}

/// What Sidekicks are owed of one Remote's Sessions.
struct OwedThere {
    /// The Pairing all of it was carried through.
    pairing: Pairing,
    acts: Vec<OwedAct>,
    /// The sequence the next act held takes.
    next_seq: u64,
    /// What is kept of each tree acted in, by the Session it is read by —
    /// the one heading it, once known — for as long as anything is owed in
    /// it, and no longer.
    trees: HashMap<SessionId, TreeKept>,
    /// The Sessions acted on there something moved in since they were last
    /// read.
    stirred: HashSet<SessionId>,
}

/// What is kept of one tree of a Remote's acted in, to tell its Reports
/// once, while anything is owed in it. None of it is more than the tree's
/// latest outline holds, and that is no more than the reach budget.
#[derive(Default)]
struct TreeKept {
    /// What the latest replay of it found owed that has been told, so none
    /// is told twice: each Turn's settling and each Intervention, to each
    /// Sidekick. What a replay no longer finds was owed by acts spent since,
    /// and is forgotten.
    told: HashSet<Owed>,
    /// The Title the last reading of it gave the Session heading it.
    title: Option<String>,
    /// Whether its last read failed while the Remote answered: read again
    /// each poll interval until one is read.
    unread: bool,
    /// Whether it has grown past what this Server reads of a Remote at
    /// once: read again each poll interval, and not each time something
    /// moves in it.
    past_budget: bool,
    /// Each Sidekick told it grew past following, so none is told twice.
    told_past_following: HashSet<SessionId>,
    /// Each Sidekick told its work left Watches running in a Session,
    /// another Report to follow: the only ones whose Watches' ending is told.
    promised: HashSet<WatchesLeft>,
    /// Each of those whose Watches a reading found ended, waking no one, by
    /// the moment on the Remote's clock the first such reading was made:
    /// told once a reading long enough after finds it so still.
    ending: HashMap<WatchesLeft, SessionTimestamp>,
    /// Each of those whose Sidekick itself stopped the Watches, owed no
    /// telling that they ended waking no one.
    stopped: HashSet<WatchesLeft>,
    /// Each Sidekick a reading found a live Watch of in a Session, by when
    /// the earliest such Watch started: what a later reading, the Watch
    /// gone, follows the Sidekick's Watches there from, for as long as a
    /// Continuation may yet come of them.
    seen: HashMap<WatchesLeft, SessionTimestamp>,
    /// Each Turn a replay took for a Sidekick's, by the Sidekick and the
    /// Turn's Session: its still, whatever a later reading no longer shows.
    claimed: HashSet<(SessionId, SessionId, TurnId)>,
}

/// A Watch an outline says is live, as the replay comes to know it.
struct OwnedWatch {
    description: String,
    started_at: SessionTimestamp,
    /// The Sidekicks whose work left it running.
    sidekicks: Vec<SessionId>,
}

/// The Watches a Sidekick's work left running in a Session, as it is owed
/// the telling of them: the Sidekick's own Session, and the Session they
/// run in.
type WatchesLeft = (SessionId, SessionId);

/// One act held, as owed Reports of.
#[derive(Clone, Debug)]
struct OwedAct {
    /// When it was held, in the order acts there were.
    seq: u64,
    sidekick: SessionId,
    owing: RemoteOwing,
}

/// Which acts held at a Remote a reading of it covers, and which trees there
/// to read for them.
pub(crate) struct RemoteReading {
    /// Every act held up to this sequence was held before the reading began,
    /// so what the reading lacks of one, it was never done.
    pub(crate) covered: u64,
    /// The Sessions to read the outlines of: one in each tree acted in.
    pub(crate) trees: Vec<SessionId>,
}

/// What one reading of a Remote's tree found: the Session heading it, what
/// is owed and not yet told, in the order it happened, and the acts it
/// spent, let go of once that is told.
#[derive(Clone, Debug, Default)]
pub(crate) struct RemoteFollowing {
    pub(crate) tree: Option<SessionId>,
    pub(crate) raises: Vec<RemoteRaise>,
    /// The acts, by their sequence, owed nothing more there.
    pub(crate) spent: Vec<u64>,
}

/// Something a replay found owed a Sidekick and not yet told: told as it
/// stands, or — a Turn's settling — once the Turn's words are read.
#[derive(Clone, Debug)]
pub(crate) enum RemoteRaise {
    /// Told as it stands, settling `told`.
    Report {
        sidekick: SessionId,
        report: SidekickReport,
        told: Vec<Owed>,
    },
    /// The Turn `turn_id` of the Session `session_id` there settled, told
    /// about `subject` once the Session is read for what its Agent wrote.
    Settled {
        sidekick: SessionId,
        session_id: SessionId,
        turn_id: TurnId,
        subject: SidekickReportSubject,
        /// What each Watch the Sidekick's work left running there, live
        /// still, is doing.
        watches: Vec<String>,
        owed: Owed,
    },
}

impl RemoteReports {
    /// Whether anything is owed of a Session of the Remote `remote`, or
    /// waits on a read there to learn whether it is.
    pub(super) fn owes_at(&self, remote: &str) -> bool {
        self.by_remote
            .get(remote)
            .is_some_and(|owed| !owed.acts.is_empty())
    }

    /// The top-level Sessions of the Remote `remote` heading what is owed
    /// there, where known.
    pub(super) fn heads_at(&self, remote: &str) -> HashSet<SessionId> {
        self.by_remote
            .get(remote)
            .into_iter()
            .flat_map(|owed| owed.acts.iter().filter_map(|act| act.owing.head))
            .collect()
    }
}

impl SessionStore {
    /// Holds `owing`, an act of the Sidekick of `sidekick` on a Session of
    /// the Remote `remote` carried through `pairing`, as owed Reports of —
    /// owing nothing until a read finds it done, where the Remote's answer to
    /// it never came back. Nothing is held for a Sidekick whose Session is no
    /// longer held; and an act carried through a Pairing that has ended
    /// since, another made by the name owing already, is told lost with it
    /// at once.
    pub(crate) fn owe_remote_reports(
        &self,
        sidekick: SessionId,
        remote: &str,
        pairing: &Pairing,
        owing: RemoteOwing,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if !state.sessions.contains_key(&sidekick) {
            return;
        }
        if state
            .remote_reports
            .by_remote
            .get(remote)
            .is_some_and(|owed| owed.pairing.generation > pairing.generation)
        {
            if owing.confirmed {
                let session = owing.head.unwrap_or(owing.session_id);
                let title = owing.title.unwrap_or_default();
                state.hold_report(
                    sidekick,
                    SidekickReport::origin_lost(
                        remote,
                        SidekickOriginLoss::Unpaired,
                        vec![(session, title)],
                    ),
                );
            }
            return;
        }
        // What was owed through a Pairing made before is that Pairing's,
        // which has ended.
        state.keep_remote_reports_pairing(remote, pairing);
        let owed = state
            .remote_reports
            .by_remote
            .entry(remote.to_owned())
            .or_insert_with(|| OwedThere {
                pairing: pairing.clone(),
                acts: Vec::new(),
                next_seq: 1,
                trees: HashMap::new(),
                stirred: HashSet::new(),
            });
        owed.stirred.insert(owing.session_id);
        // Asked again, the same act is the same act.
        if let Some(held) = owed.acts.iter_mut().find(|held| {
            held.sidekick == sidekick
                && held.owing.session_id == owing.session_id
                && held.owing.contribution == owing.contribution
        }) {
            held.owing.confirmed |= owing.confirmed;
            held.owing.head = held.owing.head.or(owing.head);
            held.owing.title = held.owing.title.take().or(owing.title);
            return;
        }
        let seq = owed.next_seq;
        owed.next_seq += 1;
        owed.acts.push(OwedAct {
            seq,
            sidekick,
            owing,
        });
    }

    /// Marks what is owed in the tree the Session `head` of the Remote
    /// `remote` heads there — or of that Session itself, where what heads it
    /// is not yet known — to be read again, something having moved in it.
    /// Answers whether anything is.
    pub(crate) fn stir_remote_reports(&self, remote: &str, head: SessionId) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return false;
        };
        // One past the budget is read again only each poll interval.
        if owed.trees.get(&head).is_some_and(|kept| kept.past_budget) {
            return false;
        }
        let stirred = owed.acts.iter().any(|act| act.tree() == head);
        if stirred {
            owed.stirred.insert(head);
        }
        stirred
    }

    /// Marks every tree of the Remote `remote` acted in to be read again —
    /// but those past the budget, read each poll interval — answering
    /// whether there is any.
    pub(crate) fn stir_all_remote_reports(&self, remote: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return false;
        };
        let trees = owed
            .acts
            .iter()
            .map(OwedAct::tree)
            .filter(|tree| !owed.trees.get(tree).is_some_and(|kept| kept.past_budget))
            .collect::<Vec<_>>();
        let stirred = !trees.is_empty();
        owed.stirred.extend(trees);
        stirred
    }

    /// Marks every tree of the Remote `remote` whose last read failed, or
    /// that grew past the budget, to be read again: the poll interval came
    /// round. Answers whether there is any.
    pub(crate) fn stir_unread_remote_reports(&self, remote: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return false;
        };
        let due = owed
            .trees
            .iter()
            .filter(|(_, kept)| kept.unread || kept.past_budget)
            .map(|(tree, _)| *tree)
            .collect::<Vec<_>>();
        owed.stirred.extend(&due);
        !due.is_empty()
    }

    /// Takes up that a read of the tree of the Remote `remote` that
    /// `read_by` belongs to, through `pairing`, failed while the Remote
    /// answered — its answer running past what this Server reads of one,
    /// where `past_budget`. That is no outage: what is owed in it is kept,
    /// and it is read again each poll interval. Each Sidekick owed Reports
    /// in a tree past the budget is told once, in plain words, that its work
    /// there cannot be followed.
    pub(crate) fn remote_tree_unread(
        &self,
        remote: &str,
        pairing: &Pairing,
        read_by: SessionId,
        past_budget: bool,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        if owed.pairing != *pairing {
            return;
        }
        let kept = owed.trees.entry(read_by).or_default();
        if !past_budget {
            kept.unread = true;
            return;
        }
        kept.unread = false;
        kept.past_budget = true;
        let mut untold = Vec::new();
        for act in &owed.acts {
            if act.tree() != read_by || !kept.told_past_following.insert(act.sidekick) {
                continue;
            }
            let title = kept
                .title
                .clone()
                .or_else(|| act.owing.title.clone())
                .unwrap_or_default();
            untold.push((act.sidekick, title));
        }
        for (sidekick, title) in untold {
            let title = if title.is_empty() {
                state
                    .remote_session_title(remote, read_by)
                    .unwrap_or_default()
            } else {
                title
            };
            state.hold_report(
                sidekick,
                SidekickReport::past_following(SidekickReportSubject {
                    session: SessionReference::new(Outlook::Remote(remote.to_owned()), read_by),
                    title,
                    subagent: None,
                }),
            );
        }
    }

    /// Which trees of the Remote `remote` to read for what is owed there —
    /// every one, where `all`, and otherwise those something moved in since
    /// they were last read — and the acts that reading covers.
    pub(crate) fn remote_reports_to_read(&self, remote: &str, all: bool) -> RemoteReading {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return RemoteReading {
                covered: 0,
                trees: Vec::new(),
            };
        };
        let stirred = std::mem::take(&mut owed.stirred);
        let mut trees = Vec::<SessionId>::new();
        for act in &owed.acts {
            let tree = act.tree();
            let moved = stirred.contains(&tree) || stirred.contains(&act.owing.session_id);
            if (all || moved) && !trees.contains(&tree) {
                trees.push(tree);
            }
        }
        RemoteReading {
            covered: owed.next_seq - 1,
            trees,
        }
    }

    /// Takes up `outline`, the tree of the Remote `remote` that the Session
    /// `read_by` belongs to, as a reading through `pairing` gave it — this
    /// Server known there by the key fingerprint `own` — the acts up to
    /// `covered` held before it was asked:
    /// replays the acts made in it through the Sidekick Report rule, and
    /// answers what that finds owed and not yet told, in the order it
    /// happened there, and the acts it spent. Each act it shows was never
    /// done, of those it covers, is let go of.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn follow_remote_outline(
        &self,
        remote: &str,
        pairing: &Pairing,
        own: &str,
        covered: u64,
        read_by: SessionId,
        outline: &SessionTreeOutline,
        wake_grace: Duration,
    ) -> RemoteFollowing {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(head) = outline.sessions.first() else {
            return RemoteFollowing::default();
        };
        let head_id = head.session.id;
        let title = head.title.clone();
        let snapshots = outline
            .sessions
            .iter()
            .map(|snapshot| (snapshot.session.id, snapshot))
            .collect::<HashMap<_, _>>();
        let held = state.sessions.keys().copied().collect::<HashSet<_>>();
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return RemoteFollowing::default();
        };
        if owed.pairing != *pairing {
            return RemoteFollowing::default();
        }
        // Read at last, it is no longer read only each poll interval.
        for tree in [read_by, head_id] {
            if let Some(kept) = owed.trees.get_mut(&tree) {
                kept.unread = false;
                kept.past_budget = false;
            }
        }
        // The acts made in this tree, each now known to be headed by it,
        // and confirmed where the outline shows it done as this Peer's.
        let mut acts = Vec::new();
        owed.acts.retain_mut(|act| {
            let in_tree = snapshots.contains_key(&act.owing.session_id)
                || act.owing.session_id == read_by
                || act.owing.head == Some(head_id);
            if !in_tree {
                return true;
            }
            act.owing.head = Some(head_id);
            act.owing.title = Some(title.clone());
            match evidence(&snapshots, act, own) {
                Evidence::Shown => act.owing.confirmed = true,
                Evidence::Pending => {}
                // Never done, as a reading asked for after it was held
                // shows: there is nothing to follow.
                Evidence::Absent if act.seq <= covered => return false,
                Evidence::Absent => {}
            }
            acts.push(act.clone());
            true
        });
        let (watched, seen, claimed) =
            owed.trees
                .get(&head_id)
                .map_or_else(Default::default, |kept| {
                    let watched = kept
                        .promised
                        .iter()
                        .chain(kept.seen.keys())
                        .copied()
                        .collect::<HashSet<_>>();
                    (watched, kept.seen.clone(), kept.claimed.clone())
                });
        let mut replay = Replay {
            snapshots: &snapshots,
            works: HashMap::new(),
            held: &held,
            watched: &watched,
            seen: &seen,
            claimed,
            owned: HashMap::new(),
            now: SessionTimestamp(0),
            liveness_known: false,
        };
        let mut found = replay.run(&acts, own);
        let remaining = replay.remaining();
        let owned = replay.owned;
        let claimed = replay.claimed;
        let kept = owed.trees.entry(head_id).or_default();
        kept.title = Some(title.clone());
        kept.claimed = claimed;
        for (session_id, watches) in &owned {
            for watch in watches {
                for sidekick in &watch.sidekicks {
                    let since = kept
                        .seen
                        .entry((*sidekick, *session_id))
                        .or_insert(watch.started_at);
                    *since = (*since).min(watch.started_at);
                }
            }
        }
        // What the replay no longer finds was owed by acts spent since.
        let finding = found.iter().cloned().collect::<HashSet<_>>();
        kept.told.retain(|told| finding.contains(told));
        // Watches ended are told only where the Sidekick was told they ran,
        // never where it stopped them itself, and those that woke no one
        // only once found so for `wake_grace`: the Continuation a Watch
        // wakes begins after the Watch is gone. Until then the Sidekick's
        // Watches there are followed still, told of or not.
        let grace = u64::try_from(wake_grace.as_millis()).unwrap_or(u64::MAX);
        // A Turn of the Sidekick's about to be told settled there is what
        // came of them: its Report says what runs on, and nothing ended.
        let settling = found
            .iter()
            .filter(|owed| !kept.told.contains(owed))
            .filter_map(|owed| match *owed {
                Owed::Settled {
                    sidekick,
                    session_id,
                    ..
                } => Some((sidekick, session_id)),
                _ => None,
            })
            .collect::<HashSet<WatchesLeft>>();
        let mut ended = HashMap::new();
        let mut awaited = HashSet::new();
        let mut over = Vec::new();
        found.retain(|owed| {
            let Owed::WatchesEnded {
                sidekick,
                session_id,
                within_another_turn,
            } = *owed
            else {
                return true;
            };
            let left = (sidekick, session_id);
            let promised = kept.promised.contains(&left);
            if settling.contains(&left) || !(promised || kept.seen.contains_key(&left)) {
                return false;
            }
            if within_another_turn {
                over.push(left);
                return promised;
            }
            let first = kept.ending.get(&left).copied().unwrap_or(outline.read_at);
            if outline.read_at.0 < first.0.saturating_add(grace) {
                ended.insert(left, first);
                awaited.insert(sidekick);
                return false;
            }
            over.push(left);
            promised && !kept.stopped.contains(&left)
        });
        kept.ending = ended;
        // Told or not, nothing more is followed of Watches over and done;
        // the telling itself spends the promise.
        for left in over {
            kept.seen.remove(&left);
            if kept.stopped.remove(&left) {
                kept.promised.remove(&left);
            }
        }
        if !awaited.is_empty() {
            kept.unread = true;
        }
        // An act whose Sidekick is owed nothing more of this tree is spent,
        // unless what it waits on may yet come: a Prompt still waiting, an
        // Answer still being submitted, or the Continuation Watches just
        // found ended may have woken.
        let spent = owed
            .acts
            .iter()
            .filter(|act| {
                act.owing.head == Some(head_id)
                    && act.seq <= covered
                    && !remaining
                        .iter()
                        .any(|(_, work)| work.sidekick == act.sidekick)
                    && !awaited.contains(&act.sidekick)
                    && evidence(&snapshots, act, own) != Evidence::Pending
            })
            .map(|act| act.seq)
            .collect();
        let kept = owed.trees.entry(head_id).or_default();
        let raises = kept.untold(found, &snapshots, &owned, remote, head_id, &title);
        owed.reclaim();
        RemoteFollowing {
            tree: Some(head_id),
            raises,
            spent,
        }
    }

    /// Tells each of `raises` — found owed by a reading of the tree of the
    /// Remote `remote` headed by `tree`, through `pairing`, and put in
    /// words, each with what it tells — to its Sidekick, held for its Agent
    /// as a Report of this Server's own Sessions is, unless it was told
    /// meanwhile or that Pairing no longer stands as it did; and lets go of
    /// the acts that reading `spent`, owed nothing more once that is told,
    /// forgetting the tree once nothing is owed in it.
    pub(crate) fn tell_remote_reports(
        &self,
        remote: &str,
        pairing: &Pairing,
        tree: Option<SessionId>,
        raises: Vec<(SessionId, SidekickReport, Vec<Owed>)>,
        spent: &[u64],
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        if owed.pairing != *pairing {
            return;
        }
        owed.acts.retain(|act| !spent.contains(&act.seq));
        let mut reports = Vec::new();
        if let Some(tree) = tree {
            let kept = owed.trees.entry(tree).or_default();
            for (sidekick, report, told) in raises {
                // That Watches ended is told while the Sidekick is still
                // promised it, and never kept as told: it may be owed again.
                let closing = told.iter().find_map(|told| match *told {
                    Owed::WatchesEnded {
                        sidekick,
                        session_id,
                        ..
                    } => Some((sidekick, session_id)),
                    _ => None,
                });
                if let Some(left) = closing {
                    if kept.promised.remove(&left) {
                        kept.ending.remove(&left);
                        kept.stopped.remove(&left);
                        kept.seen.remove(&left);
                        reports.push((sidekick, report));
                    }
                    continue;
                }
                if told.iter().all(|told| kept.told.contains(told)) {
                    continue;
                }
                // A settled Turn's Report says whether another follows: of
                // the Watches there, and of every Session the Sidekick's
                // Watches were found live in, a Subagent's among them.
                for told in &told {
                    if let Owed::Settled {
                        sidekick,
                        session_id,
                        ..
                    } = *told
                    {
                        let left = (sidekick, session_id);
                        if report.promises_another() {
                            let lefts = kept
                                .seen
                                .keys()
                                .filter(|(held, _)| *held == sidekick)
                                .copied()
                                .chain([left])
                                .collect::<Vec<_>>();
                            for left in lefts {
                                kept.promised.insert(left);
                                kept.stopped.remove(&left);
                            }
                        } else {
                            kept.promised.remove(&left);
                            kept.ending.remove(&left);
                            kept.stopped.remove(&left);
                            kept.seen.remove(&left);
                        }
                    }
                }
                kept.told.extend(told);
                reports.push((sidekick, report));
            }
        }
        owed.reclaim();
        if owed.acts.is_empty() {
            state.remote_reports.by_remote.remove(remote);
        }
        for (sidekick, report) in reports {
            state.hold_report(sidekick, report);
        }
    }

    /// Lets go of every act on a Session of the tree of the Remote
    /// `remote` that `read_by` belongs to, of those up to `covered`: a read
    /// there found the Remote holds no such Session, or cannot read it, so
    /// there is nothing more to follow, and nothing to tell.
    pub(crate) fn let_go_of_remote_reports(&self, remote: &str, read_by: SessionId, covered: u64) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        owed.acts.retain(|act| {
            act.seq > covered
                || (act.owing.session_id != read_by && act.owing.head != Some(read_by))
        });
        owed.reclaim();
        if owed.acts.is_empty() {
            state.remote_reports.by_remote.remove(remote);
        }
    }

    /// Takes up that the Sidekick of `sidekick` itself stopped the Watches
    /// of the Session `session_id` of the Remote `remote` — and, where it
    /// heads its tree, of every Session below it: it is owed no telling that
    /// Watches its own work left running there ended waking no one, having
    /// ended them.
    pub(crate) fn remote_sidekick_stopped_watches(
        &self,
        remote: &str,
        sidekick: SessionId,
        session_id: SessionId,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        let trees = owed
            .acts
            .iter()
            .filter(|act| act.owing.session_id == session_id || act.owing.head == Some(session_id))
            .map(OwedAct::tree)
            .collect::<HashSet<_>>();
        for tree in trees {
            if let Some(kept) = owed.trees.get_mut(&tree) {
                let stopped = kept
                    .promised
                    .iter()
                    .chain(kept.seen.keys())
                    .filter(|(held, there)| {
                        *held == sidekick && (*there == session_id || tree == session_id)
                    })
                    .copied()
                    .collect::<Vec<_>>();
                kept.stopped.extend(stopped);
            }
        }
    }

    /// Takes up that the Remote `remote` stopped answering through a
    /// Pairing of a generation up to `ended`, or that such a Pairing ended,
    /// for `loss`: each Sidekick owed Reports through it is told so once,
    /// naming the Sessions it was waiting on, and everything owed through it
    /// ends. What was owed through a Pairing made after is that Pairing's,
    /// and is left. An act whose answer never came back, which owes nothing
    /// yet, waits still for a read to tell where the Remote only stopped
    /// answering; a Pairing ended takes it too.
    pub(crate) fn remote_reports_lost(&self, remote: &str, ended: u64, loss: SidekickOriginLoss) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state
            .remote_reports
            .by_remote
            .get(remote)
            .is_some_and(|owed| owed.pairing.generation > ended)
        {
            return;
        }
        state.lose_remote_reports(remote, loss);
    }
}

#[cfg(test)]
impl SessionStore {
    /// The trees of the Remote `remote` anything is kept of to tell their
    /// Reports once.
    fn remote_trees_kept(&self, remote: &str) -> HashSet<SessionId> {
        let state = self.state.lock().unwrap();
        let Some(owed) = state.remote_reports.by_remote.get(remote) else {
            return HashSet::new();
        };
        owed.trees.keys().copied().collect()
    }
}

impl OwedAct {
    /// The tree it was made in, by the Session that tree is read by: the one
    /// heading it, once known, and otherwise the Session acted on.
    fn tree(&self) -> SessionId {
        self.owing.head.unwrap_or(self.owing.session_id)
    }
}

impl OwedThere {
    /// Forgets what is kept of each tree nothing is owed in any longer.
    fn reclaim(&mut self) {
        let acts = &self.acts;
        self.trees
            .retain(|tree, _| acts.iter().any(|act| act.tree() == *tree));
    }
}

impl TreeKept {
    /// What of `found` — owed, in the order it happened in the tree headed
    /// by `head`, titled `title`, of the Remote `remote`, whose Sessions are
    /// `snapshots` — was not told before, as what tells it: Interventions
    /// one by one up to so many for each Sidekick, any more counted in one
    /// Report, and each Turn's settling to be put in words, with what of
    /// `owned` — the Watches live in each Session, each with the Sidekicks
    /// whose work left it running — its Sidekick's work left where it
    /// settled.
    fn untold(
        &self,
        found: Vec<Owed>,
        snapshots: &HashMap<SessionId, &SessionSnapshot>,
        owned: &HashMap<SessionId, Vec<OwnedWatch>>,
        remote: &str,
        head: SessionId,
        title: &str,
    ) -> Vec<RemoteRaise> {
        let subject = |session_id: SessionId| SidekickReportSubject {
            session: SessionReference::new(Outlook::Remote(remote.to_owned()), head),
            title: title.to_owned(),
            subagent: (session_id != head).then_some(session_id),
        };
        let mut raises = Vec::new();
        let mut told_one_by_one = HashMap::<SessionId, usize>::new();
        let mut uncounted = Vec::<(SessionId, Vec<Owed>)>::new();
        for owed in found {
            if self.told.contains(&owed) {
                continue;
            }
            match owed {
                Owed::Settled {
                    sidekick,
                    session_id,
                    turn_id,
                } => raises.push(RemoteRaise::Settled {
                    sidekick,
                    session_id,
                    turn_id,
                    subject: subject(session_id),
                    watches: {
                        // Those live there, and in each Session beneath it.
                        let mut watches = owned
                            .iter()
                            .filter(|(there, _)| {
                                let mut at = Some(**there);
                                let mut remaining = snapshots.len();
                                while let Some(current) = at {
                                    if current == session_id {
                                        return true;
                                    }
                                    let Some(left) = remaining.checked_sub(1) else {
                                        break;
                                    };
                                    remaining = left;
                                    at = snapshots
                                        .get(&current)
                                        .and_then(|snapshot| snapshot.session.parent);
                                }
                                false
                            })
                            .flat_map(|(_, watches)| watches)
                            .filter(|watch| watch.sidekicks.contains(&sidekick))
                            .collect::<Vec<_>>();
                        watches.sort_by_key(|watch| watch.started_at);
                        watches
                            .into_iter()
                            .map(|watch| watch.description.clone())
                            .collect()
                    },
                    owed,
                }),
                Owed::Asked {
                    sidekick,
                    session_id,
                    activity_id,
                    intervention,
                } => {
                    let told = told_one_by_one.entry(sidekick).or_default();
                    if *told >= INTERVENTIONS_TOLD_AT_ONCE {
                        match uncounted.iter_mut().find(|(held, _)| *held == sidekick) {
                            Some((_, more)) => more.push(owed),
                            None => uncounted.push((sidekick, vec![owed])),
                        }
                        continue;
                    }
                    *told += 1;
                    let waiting = snapshots
                        .get(&session_id)
                        .is_some_and(|snapshot| waits(snapshot, activity_id));
                    raises.push(RemoteRaise::Report {
                        sidekick,
                        report: SidekickReport::intervention_asked(
                            subject(session_id),
                            intervention,
                            waiting,
                        ),
                        told: vec![owed],
                    });
                }
                Owed::WatchesEnded {
                    sidekick,
                    session_id,
                    within_another_turn,
                    ..
                } => raises.push(RemoteRaise::Report {
                    sidekick,
                    report: SidekickReport::watches_ended(subject(session_id), within_another_turn),
                    told: vec![owed],
                }),
            }
        }
        for (sidekick, told) in uncounted {
            raises.push(RemoteRaise::Report {
                sidekick,
                report: SidekickReport::more_interventions(subject(head), told.len()),
                told,
            });
        }
        raises
    }
}

impl SessionStoreState {
    /// See [`SessionStore::remote_reports_lost`]: whatever the Pairing.
    pub(super) fn lose_remote_reports(&mut self, remote: &str, loss: SidekickOriginLoss) {
        let Some(mut owed) = self.remote_reports.by_remote.remove(remote) else {
            return;
        };
        let mut told = Vec::<(SessionId, Vec<(SessionId, String)>)>::new();
        for act in owed.acts.iter().filter(|act| act.owing.confirmed) {
            let session = act.tree();
            let title = owed
                .trees
                .get(&session)
                .and_then(|kept| kept.title.clone())
                .or_else(|| act.owing.title.clone())
                .or_else(|| self.remote_session_title(remote, session))
                .unwrap_or_default();
            let at = match told
                .iter()
                .position(|(sidekick, _)| *sidekick == act.sidekick)
            {
                Some(at) => at,
                None => {
                    told.push((act.sidekick, Vec::new()));
                    told.len() - 1
                }
            };
            let sessions = &mut told[at].1;
            if !sessions.iter().any(|(held, _)| *held == session) {
                sessions.push((session, title));
            }
        }
        for (sidekick, sessions) in told {
            self.hold_report(
                sidekick,
                SidekickReport::origin_lost(remote, loss, sessions),
            );
        }
        if loss == SidekickOriginLoss::StoppedAnswering {
            owed.acts.retain(|act| !act.owing.confirmed);
            if !owed.acts.is_empty() {
                owed.stirred.clear();
                owed.trees.clear();
                self.remote_reports
                    .by_remote
                    .insert(remote.to_owned(), owed);
            }
        }
    }

    /// Lets go of everything owed of the Session `session_id` of the Remote
    /// `remote`, or beneath it there: it is gone, so nothing more will be
    /// said of it.
    pub(super) fn let_go_of_remote_session_reports(&mut self, remote: &str, session_id: SessionId) {
        let Some(owed) = self.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        owed.acts
            .retain(|act| act.owing.session_id != session_id && act.owing.head != Some(session_id));
        owed.reclaim();
        if owed.acts.is_empty() {
            self.remote_reports.by_remote.remove(remote);
        }
    }

    /// Drops everything owed at Remotes to a Sidekick whose Session is among
    /// `deleted`: there is no Agent left to tell.
    pub(super) fn forget_remote_sidekicks(&mut self, deleted: &[SessionId]) {
        self.remote_reports.by_remote.retain(|_, owed| {
            owed.acts.retain(|act| !deleted.contains(&act.sidekick));
            owed.reclaim();
            !owed.acts.is_empty()
        });
    }

    /// The Remote `remote` was reached through `pairing`: whatever was owed
    /// there through a Pairing made before it has ended, and is lost with
    /// it.
    pub(super) fn keep_remote_reports_pairing(&mut self, remote: &str, pairing: &Pairing) {
        if self
            .remote_reports
            .by_remote
            .get(remote)
            .is_some_and(|owed| owed.pairing.generation < pairing.generation)
        {
            self.lose_remote_reports(remote, SidekickOriginLoss::Unpaired);
        }
    }
}

/// What a reading of a Remote shows of whether an act was done.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Evidence {
    /// Done, as this Peer's: its Prompt stands in the Session, or its
    /// Answer was delivered as that very act.
    Shown,
    /// Not yet known: an Answer still waiting on its Questionnaire, or being
    /// submitted.
    Pending,
    /// Not done: no such Prompt stands there as this Peer's, or the
    /// Questionnaire settled otherwise.
    Absent,
}

/// What `snapshots` show of whether `act` was done, this Server known to the
/// Remote by the key fingerprint `own`.
fn evidence(
    snapshots: &HashMap<SessionId, &SessionSnapshot>,
    act: &OwedAct,
    own: &str,
) -> Evidence {
    match snapshots.get(&act.owing.session_id) {
        Some(snapshot) => shown(snapshot, act.owing.contribution, own),
        None => Evidence::Absent,
    }
}

/// What `snapshot`, the Session an act was made in, shows of whether the act
/// that left `contribution` was done, this Server known to the Remote by the
/// key fingerprint `own`.
pub(super) fn shown(
    snapshot: &SessionSnapshot,
    contribution: RemoteContribution,
    own: &str,
) -> Evidence {
    match contribution {
        RemoteContribution::Prompt(prompt_id) => {
            let ours = snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == prompt_id && of_this_peer(prompt.author.as_ref(), own));
            if ours {
                Evidence::Shown
            } else {
                Evidence::Absent
            }
        }
        RemoteContribution::Answer { questionnaire, act } => {
            let Some((outcome, author)) =
                snapshot
                    .activities
                    .iter()
                    .find_map(|activity| match activity {
                        Activity::Questionnaire {
                            questionnaire: asked,
                            outcome,
                            author,
                            ..
                        } if asked.id == questionnaire => Some((*outcome, author.as_ref())),
                        _ => None,
                    })
            else {
                return Evidence::Absent;
            };
            match outcome {
                QuestionnaireOutcome::Answered if answered_as(author, own, act) => Evidence::Shown,
                // Whose submission it is is not said until it settles.
                QuestionnaireOutcome::Pending | QuestionnaireOutcome::Submitting => {
                    Evidence::Pending
                }
                _ => Evidence::Absent,
            }
        }
    }
}

/// Whether `author` is a Sidekick on this Peer, known to the Remote by the
/// key fingerprint `own`.
fn of_this_peer(author: Option<&Author>, own: &str) -> bool {
    matches!(author, Some(Author::PeerSidekick { fingerprint, .. }) if fingerprint == own)
}

/// Whether `author` is this Peer's act `act`.
fn answered_as(author: Option<&Author>, own: &str, act: ActId) -> bool {
    matches!(
        author,
        Some(Author::PeerSidekick { fingerprint, act: Some(answered), .. })
            if fingerprint == own && *answered == act
    )
}

/// Whether the Intervention `activity_id` of `snapshot` waits still.
fn waits(snapshot: &SessionSnapshot, activity_id: ActivityId) -> bool {
    snapshot.activities.iter().any(|activity| match activity {
        Activity::Questionnaire { id, outcome, .. } => {
            *id == activity_id && *outcome == QuestionnaireOutcome::Pending
        }
        Activity::Approval { id, outcome, .. } => {
            *id == activity_id && *outcome == ApprovalOutcome::Pending
        }
        _ => false,
    })
}

/// One thing that happened in a Remote's tree, as the rule takes it up.
#[derive(Clone, Copy, Debug)]
enum Happened {
    Taken {
        session_id: SessionId,
        prompt_id: PromptId,
        turn_id: TurnId,
    },
    Began {
        session_id: SessionId,
        turn_id: TurnId,
    },
    /// The Watch at `index` of those the outline says are live in
    /// `session_id` started.
    WatchStarted { session_id: SessionId, index: usize },
    /// A Watch of the Sidekick of `sidekick`, found live in `session_id` by
    /// an earlier reading, started.
    WatchSeen {
        session_id: SessionId,
        sidekick: SessionId,
    },
    Answered {
        session_id: SessionId,
        sidekick: SessionId,
        turn_id: TurnId,
    },
    Asked {
        session_id: SessionId,
        turn_id: TurnId,
        activity_id: ActivityId,
        intervention: SidekickIntervention,
        at: SessionTimestamp,
    },
    Settled {
        session_id: SessionId,
        turn_id: TurnId,
        at: SessionTimestamp,
    },
}

/// A Remote's tree as one outline gives it, and the work the replay of the
/// acts made in it holds there: the [`WorkTree`] the Sidekick Report rule
/// follows a Remote's Sessions through.
struct Replay<'a> {
    snapshots: &'a HashMap<SessionId, &'a SessionSnapshot>,
    works: HashMap<SessionId, Vec<SidekickWork>>,
    /// The Sidekicks' own Sessions this Server holds.
    held: &'a HashSet<SessionId>,
    /// Each Sidekick an earlier reading found Watches of live in a Session,
    /// or told so.
    watched: &'a HashSet<WatchesLeft>,
    /// Those found live, by when the earliest started.
    seen: &'a HashMap<WatchesLeft, SessionTimestamp>,
    /// Each Turn a replay took for a Sidekick's, by the Sidekick and the
    /// Turn's Session: those of earlier readings, and this one's.
    claimed: HashSet<(SessionId, SessionId, TurnId)>,
    /// The Watches the outline says are live in each Session that the
    /// replay has come to the start of.
    owned: HashMap<SessionId, Vec<OwnedWatch>>,
    /// The moment the replay has come to, on the Remote's clock.
    now: SessionTimestamp,
    /// Whether whether a Session works is known: not while what happened is
    /// replayed, since the outline says only how each stands now; once it
    /// is, at the moment the outline was read.
    liveness_known: bool,
}

impl WorkTree for Replay<'_> {
    fn snapshot(&self, session_id: SessionId) -> Option<&SessionSnapshot> {
        self.snapshots.get(&session_id).copied()
    }

    fn works_now(&self, session_id: SessionId) -> Option<bool> {
        if !self.liveness_known {
            return None;
        }
        Some(
            self.snapshots
                .get(&session_id)
                .is_some_and(|snapshot| snapshot.session.working_since.is_some()),
        )
    }

    /// A Watch live as the outline was read has been live ever since it
    /// started. Of one no longer live, no outline says when it ended: so
    /// where something shows one ran on — the Sidekick was told of it by an
    /// earlier reading, or a Watch Outcome stands in a Turn begun since the
    /// moment replayed — whether it was live at a moment before the
    /// outline's own is not known, and where nothing does, none was.
    fn watches_live(&self, session_id: SessionId, sidekick: SessionId) -> Option<bool> {
        let live = self.owned.get(&session_id).is_some_and(|watches| {
            watches
                .iter()
                .any(|watch| watch.sidekicks.contains(&sidekick))
        });
        if live {
            return Some(true);
        }
        if self.liveness_known {
            return Some(false);
        }
        let ran_on = self.watched.contains(&(sidekick, session_id))
            || self.snapshots.get(&session_id).is_some_and(|snapshot| {
                snapshot.activities.iter().any(|activity| {
                    matches!(
                        activity,
                        Activity::WatchOutcome { turn_id, .. }
                            if snapshot.turns.iter().any(|turn| {
                                turn.id == *turn_id
                                    && turn.started_at.is_some_and(|began| began > self.now)
                            })
                    )
                })
            });
        if ran_on { None } else { Some(false) }
    }

    fn works(&self, session_id: SessionId) -> &[SidekickWork] {
        self.works.get(&session_id).map_or(&[], Vec::as_slice)
    }

    fn works_mut(&mut self, session_id: SessionId) -> Option<&mut Vec<SidekickWork>> {
        self.snapshots
            .contains_key(&session_id)
            .then(|| self.works.entry(session_id).or_default())
    }

    fn sidekick_held(&self, sidekick: SessionId) -> bool {
        self.held.contains(&sidekick)
    }

    fn bound(&self) -> usize {
        self.snapshots.len()
    }
}

impl Replay<'_> {
    /// Replays `acts` through the rule, in the order the Remote's clock says
    /// what came of them happened, this Server known there by the key
    /// fingerprint `own`, answering everything found owed, in that order;
    /// then lets go of what the outline says has ended.
    fn run(&mut self, acts: &[OwedAct], own: &str) -> Vec<Owed> {
        for act in acts {
            if let (RemoteContribution::Prompt(prompt_id), true) =
                (act.owing.contribution, act.owing.confirmed)
                && self.held.contains(&act.sidekick)
                && let Some(works) = self.works_mut(act.owing.session_id)
            {
                let sent = SidekickWork::sent(act.sidekick, prompt_id);
                if !works.contains(&sent) {
                    works.push(sent);
                }
            }
        }
        let mut found = Vec::new();
        for (at, happened) in self.happened(acts, own) {
            self.now = at;
            match happened {
                Happened::Taken {
                    session_id,
                    prompt_id,
                    turn_id,
                } => {
                    take_prompt(self, session_id, prompt_id, turn_id);
                    self.adopt_watches(session_id, turn_id);
                }
                Happened::Began {
                    session_id,
                    turn_id,
                } => {
                    // A Turn an earlier reading took for a Sidekick's is
                    // its still, whatever showed it so then.
                    let claimed = self
                        .claimed
                        .iter()
                        .filter(|(sidekick, there, turn)| {
                            *there == session_id && *turn == turn_id && self.held.contains(sidekick)
                        })
                        .map(|(sidekick, _, _)| *sidekick)
                        .collect::<Vec<_>>();
                    if let Some(works) = self.works_mut(session_id) {
                        for sidekick in claimed {
                            let working = SidekickWork::working(sidekick, turn_id);
                            if !works.contains(&working) {
                                works.push(working);
                            }
                        }
                    }
                    found.extend(turn_began(self, session_id, turn_id));
                    let taken = self
                        .works(session_id)
                        .iter()
                        .filter(|work| work.working_turn() == Some(turn_id))
                        .map(|work| (work.sidekick, session_id, turn_id))
                        .collect::<Vec<_>>();
                    self.claimed.extend(taken);
                }
                Happened::WatchSeen {
                    session_id,
                    sidekick,
                } => {
                    if self.held.contains(&sidekick) {
                        follow_watches(self, session_id, sidekick);
                    }
                }
                Happened::WatchStarted { session_id, index } => {
                    let Some(snapshot) = self.snapshots.get(&session_id).copied() else {
                        continue;
                    };
                    let watch = &snapshot.watches[index];
                    let active = snapshot
                        .turns
                        .iter()
                        .rev()
                        .find(|turn| started_within(turn, watch.started_at))
                        .map(|turn| turn.id);
                    let sidekicks = watch_started(self, session_id, active, Some(watch.started_at));
                    self.owned.entry(session_id).or_default().push(OwnedWatch {
                        description: watch.description.clone(),
                        started_at: watch.started_at,
                        sidekicks,
                    });
                }
                Happened::Answered {
                    session_id,
                    sidekick,
                    turn_id,
                } => {
                    answer_delivered(self, session_id, sidekick, turn_id);
                    self.adopt_watches(session_id, turn_id);
                }
                // Each judged by who had set what working as it happened,
                // whatever has been set working since.
                Happened::Asked {
                    session_id,
                    turn_id,
                    activity_id,
                    intervention,
                    at,
                } => found.extend(intervention_asked(
                    self,
                    session_id,
                    turn_id,
                    activity_id,
                    intervention,
                    Some(at),
                )),
                Happened::Settled {
                    session_id,
                    turn_id,
                    at,
                } => found.extend(turns_settled(self, session_id, &[turn_id], Some(at))),
            }
        }
        self.liveness_known = true;
        let sessions = self.snapshots.keys().copied().collect::<Vec<_>>();
        for session_id in sessions {
            let_go_of_untaken_prompts(self, session_id);
            let_go_of_settled_branches(self, session_id);
            found.extend(let_go_of_ended_watches(self, session_id));
        }
        found
    }

    /// Makes each Watch live in `session_id` that started while its Turn
    /// `turn_id` worked the work of every Sidekick whose Turn that has since
    /// become — its Prompt taken into it as a steer, or its Answer delivered
    /// in it — as one started after would be.
    fn adopt_watches(&mut self, session_id: SessionId, turn_id: TurnId) {
        let Some(turn) = self
            .snapshots
            .get(&session_id)
            .and_then(|snapshot| snapshot.turns.iter().find(|turn| turn.id == turn_id))
        else {
            return;
        };
        let sidekicks = self
            .works(session_id)
            .iter()
            .filter(|work| work.working_turn() == Some(turn_id))
            .map(|work| work.sidekick)
            .collect::<Vec<_>>();
        let mut adopted = Vec::new();
        for watch in self.owned.entry(session_id).or_default() {
            if !started_within(turn, watch.started_at) {
                continue;
            }
            for sidekick in &sidekicks {
                if !watch.sidekicks.contains(sidekick) {
                    watch.sidekicks.push(*sidekick);
                    adopted.push(*sidekick);
                }
            }
        }
        for sidekick in adopted {
            follow_watches(self, session_id, sidekick);
        }
    }

    /// Everything the outline says happened that the rule takes up, in the
    /// order the Remote's one clock stamped it: a taking, a beginning, a
    /// delivery, an asking and a settling stamped in one moment in that
    /// order, as one commit there would have them.
    fn happened(&self, acts: &[OwedAct], own: &str) -> Vec<(SessionTimestamp, Happened)> {
        let mut happened = Vec::<(SessionTimestamp, u8, Happened)>::new();
        let at = |stamp: Option<SessionTimestamp>| stamp.unwrap_or(SessionTimestamp(0));
        for snapshot in self.snapshots.values() {
            let session_id = snapshot.session.id;
            for prompt in &snapshot.prompts {
                if let Some(taking) = prompt.taken
                    && prompt.status == PromptStatus::Delivered
                {
                    happened.push((
                        at(taking.taken_at),
                        0,
                        Happened::Taken {
                            session_id,
                            prompt_id: prompt.id,
                            turn_id: taking.turn_id,
                        },
                    ));
                }
            }
            for activity in &snapshot.activities {
                match activity {
                    Activity::Questionnaire {
                        id,
                        turn_id,
                        questionnaire,
                        outcome,
                        author,
                        asked_at,
                        settled_at,
                        ..
                    } => {
                        happened.push((
                            at(*asked_at),
                            3,
                            Happened::Asked {
                                session_id,
                                turn_id: *turn_id,
                                activity_id: *id,
                                intervention: SidekickIntervention::Questionnaire,
                                at: at(*asked_at),
                            },
                        ));
                        if *outcome != QuestionnaireOutcome::Answered {
                            continue;
                        }
                        // The Answer delivered, where it was a Sidekick's
                        // act here, to that Sidekick.
                        for act in acts {
                            if let RemoteContribution::Answer {
                                questionnaire: answered,
                                act: act_id,
                            } = act.owing.contribution
                                && answered == questionnaire.id
                                && act.owing.session_id == session_id
                                && answered_as(author.as_ref(), own, act_id)
                            {
                                happened.push((
                                    at(*settled_at),
                                    2,
                                    Happened::Answered {
                                        session_id,
                                        sidekick: act.sidekick,
                                        turn_id: *turn_id,
                                    },
                                ));
                            }
                        }
                    }
                    Activity::Approval {
                        id,
                        turn_id,
                        asked_at,
                        ..
                    } => happened.push((
                        at(*asked_at),
                        3,
                        Happened::Asked {
                            session_id,
                            turn_id: *turn_id,
                            activity_id: *id,
                            intervention: SidekickIntervention::Approval,
                            at: at(*asked_at),
                        },
                    )),
                    _ => {}
                }
            }
            for ((sidekick, there), since) in self.seen {
                if *there == session_id {
                    happened.push((
                        *since,
                        1,
                        Happened::WatchSeen {
                            session_id,
                            sidekick: *sidekick,
                        },
                    ));
                }
            }
            for (index, watch) in snapshot.watches.iter().enumerate() {
                happened.push((
                    watch.started_at,
                    1,
                    Happened::WatchStarted { session_id, index },
                ));
            }
            for turn in &snapshot.turns {
                happened.push((
                    at(turn.started_at),
                    1,
                    Happened::Began {
                        session_id,
                        turn_id: turn.id,
                    },
                ));
                if turn.status.is_terminal() {
                    happened.push((
                        at(turn.settled_at),
                        4,
                        Happened::Settled {
                            session_id,
                            turn_id: turn.id,
                            at: at(turn.settled_at),
                        },
                    ));
                }
            }
        }
        happened.sort_by_key(|(stamp, order, _)| (*stamp, *order));
        happened
            .into_iter()
            .map(|(at, _, happened)| (at, happened))
            .collect()
    }

    /// The work the replay leaves owed, by the Session holding it.
    fn remaining(&self) -> Vec<(SessionId, SidekickWork)> {
        let mut remaining = self
            .works
            .iter()
            .flat_map(|(session_id, works)| works.iter().map(|work| (*session_id, *work)))
            .collect::<Vec<_>>();
        remaining.sort_by_key(|(session_id, _)| session_id.as_uuid());
        remaining
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::protocol::{
        AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, Answer, CreateSessionRequest,
        ExecutionDirectory, InitialPrompt, ModelId, PromptDelivery, ProviderId, Questionnaire,
        SessionChange, WatchOutcomeStatus,
    };
    use crate::provider::ProviderWatchId;
    use crate::questionnaire::Question;
    use crate::sessions::{
        DeliveredTurnStatus, ProviderTurnOutcome, StoreOutcome, TrailingCommandOutput,
    };
    use crate::storage::{RestoredSessions, StorageRepository, StorageWriter};

    /// The Remote every act here was carried to, the key of the Pairing it
    /// was carried through, and this Server as the Remote knows it.
    const STUDIO: &str = "studio";
    const PAIRING: &str = "SHA256:studio";
    const OWN: &str = "own-key";

    /// The Pairing with [`STUDIO`] of the generation `generation`, by the
    /// one key it is ever paired to here.
    fn paired(generation: u64) -> Pairing {
        Pairing {
            fingerprint: PAIRING.to_owned(),
            generation,
        }
    }

    fn asking(text: &str) -> InitialPrompt {
        InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        }
    }

    async fn empty_store(directory: &Path) -> (StorageWriter, SessionStore) {
        let repository = StorageRepository::open(directory).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(
            RestoredSessions::default(),
            sink,
            Vec::new(),
            Default::default(),
        );
        (writer, store)
    }

    /// A Session begun in `workspace` asking `text`, its first Turn begun,
    /// and that Turn. The store's own Sessions stand for a Remote's here:
    /// what is owed of a Remote's tree is taken up from its outline,
    /// whoever reads it.
    fn working(store: &SessionStore, workspace: &Path, text: &str) -> (SessionId, TurnId) {
        let StoreOutcome::Created(snapshot) = store
            .create(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: ExecutionDirectory {
                    path: workspace.to_owned(),
                },
                prompt: asking(text),
            })
            .unwrap()
        else {
            panic!("the Session is begun afresh");
        };
        let session_id = snapshot.session.id;
        let turn_id = store
            .deliver_prompt(
                session_id,
                snapshot.prompts[0].id,
                None,
                DeliveredTurnStatus::Active,
            )
            .unwrap()
            .expect("the Prompt begins a Turn")
            .turn_id;
        (session_id, turn_id)
    }

    /// A Sidekick on this Peer as the Remote names it, in its act `act`.
    fn of_this_peer(act: ActId) -> Author {
        Author::PeerSidekick {
            peer: "laptop".to_owned(),
            fingerprint: OWN.to_owned(),
            act: Some(act),
        }
    }

    /// Admits `text` to `session_id` as a steer this Peer's Sidekick sent
    /// there, answering its Prompt.
    fn steered_by_this_peer(store: &SessionStore, session_id: SessionId, text: &str) -> PromptId {
        let StoreOutcome::Created(admission) = store
            .admit(
                session_id,
                AdmitPromptRequest {
                    prompt: asking(text),
                    delivery: PromptDelivery::Steer,
                },
                Vec::new(),
                Some(of_this_peer(ActId::new())),
            )
            .unwrap()
        else {
            panic!("the steer is admitted afresh");
        };
        admission.prompt.id
    }

    /// Has `session_id`'s Turn `turn_id` ask a Questionnaire, answering it.
    fn asks(
        store: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> (ActivityId, QuestionnaireId) {
        let id = ActivityId::new();
        let questionnaire = QuestionnaireId::new();
        store
            .publish(
                session_id,
                vec![SessionChange::ActivityAdded {
                    activity: Activity::Questionnaire {
                        id,
                        turn_id,
                        questionnaire: Questionnaire {
                            id: questionnaire,
                            questions: vec![Question {
                                id: "machine".to_owned(),
                                title: None,
                                text: "Where should the tests run?".to_owned(),
                                choices: Vec::new(),
                                multiple: false,
                                freeform: true,
                                combine_freeform: false,
                                secret: false,
                                required: true,
                            }],
                        },
                        outcome: QuestionnaireOutcome::Pending,
                        answer: None,
                        author: None,
                        asked_at: None,
                        settled_at: None,
                    },
                }],
            )
            .unwrap();
        (id, questionnaire)
    }

    /// Takes a submission to the Questionnaire `activity_id` of
    /// `session_id`, and settles it answered by `author`.
    fn answered(
        store: &SessionStore,
        session_id: SessionId,
        activity_id: ActivityId,
        author: Option<Author>,
    ) {
        store
            .publish(
                session_id,
                vec![SessionChange::QuestionnaireAccepted { activity_id }],
            )
            .unwrap();
        settles(store, session_id, activity_id, author);
    }

    /// Settles the Questionnaire `activity_id` of `session_id`, its
    /// submission taken, answered by `author`.
    fn settles(
        store: &SessionStore,
        session_id: SessionId,
        activity_id: ActivityId,
        author: Option<Author>,
    ) {
        store
            .publish(
                session_id,
                vec![SessionChange::QuestionnaireSettled {
                    activity_id,
                    outcome: QuestionnaireOutcome::Answered,
                    answer: Some(Answer {
                        questions: Vec::new(),
                    }),
                    author,
                    settled_at: None,
                }],
            )
            .unwrap();
    }

    /// Settles `session_id`'s Turn `turn_id` as completed.
    fn completes(store: &SessionStore, session_id: SessionId, turn_id: TurnId) {
        store
            .finish_provider_turn(
                session_id,
                turn_id,
                ProviderTurnOutcome::Completed {
                    trailing_output: TrailingCommandOutput::new(),
                },
            )
            .unwrap();
    }

    /// Reads the outline of `session_id`'s tree and takes it up as one of
    /// the Remote's, telling what it finds owed — a Turn's settling read from
    /// the store's own Session — and answers the Reports held for `sidekick`.
    async fn read(
        store: &SessionStore,
        session_id: SessionId,
        covered: u64,
        sidekick: SessionId,
    ) -> Vec<String> {
        read_waiting(
            store,
            session_id,
            covered,
            sidekick,
            Duration::from_secs(3600),
        )
        .await
    }

    /// As [`read`], waiting `wake_grace` on Watches found ended, having
    /// woken no one, before telling that they did.
    async fn read_waiting(
        store: &SessionStore,
        session_id: SessionId,
        covered: u64,
        sidekick: SessionId,
        wake_grace: Duration,
    ) -> Vec<String> {
        let outline = store
            .tree_outline(session_id)
            .await
            .unwrap()
            .expect("the tree is held");
        let following = store.follow_remote_outline(
            STUDIO,
            &paired(1),
            OWN,
            covered,
            session_id,
            &outline,
            wake_grace,
        );
        let told = following
            .raises
            .into_iter()
            .filter_map(|raise| match raise {
                RemoteRaise::Report {
                    sidekick,
                    report,
                    told,
                } => Some((sidekick, report, told)),
                RemoteRaise::Settled {
                    sidekick,
                    session_id,
                    turn_id,
                    subject,
                    watches,
                    owed,
                } => {
                    let snapshot = store.snapshot(session_id)?;
                    let turn = snapshot.turns.iter().find(|turn| turn.id == turn_id)?;
                    let report =
                        crate::sessions::settled_report(subject, &snapshot, turn, watches)?;
                    Some((sidekick, report, vec![owed]))
                }
            })
            .collect();
        store.tell_remote_reports(STUDIO, &paired(1), following.tree, told, &following.spent);
        store
            .take_held_reports(sidekick)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Holds `contribution` of `sidekick`'s to `session_id` as owed at the
    /// Remote, answering the sequence a reading begun now covers.
    fn owe(
        store: &SessionStore,
        sidekick: SessionId,
        session_id: SessionId,
        contribution: RemoteContribution,
        confirmed: bool,
    ) -> u64 {
        store.owe_remote_reports(
            sidekick,
            STUDIO,
            &paired(1),
            RemoteOwing {
                session_id,
                head: None,
                title: None,
                contribution,
                confirmed,
            },
        );
        store.remote_reports_to_read(STUDIO, true).covered
    }

    /// Review item 2: a reading answers only for the acts held before it
    /// began. A Prompt held while the reading was on its way is missing from
    /// what it read, and is not let go of for that; a reading begun after it
    /// that still does not find it does let it go.
    #[tokio::test]
    async fn a_reading_lets_go_only_of_acts_held_before_it_began() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, turn) = working(&store, &workspace, "Run the auth suite.");
        let steered = steered_by_this_peer(&store, there, "Fix the flaky login test.");
        let covered = owe(
            &store,
            sidekick,
            there,
            RemoteContribution::Prompt(steered),
            true,
        );
        // Held after that reading began, and never admitted there.
        let unread = PromptId::new();
        owe(
            &store,
            sidekick,
            there,
            RemoteContribution::Prompt(unread),
            false,
        );
        let state_of = |covered| {
            let state = store.state.lock().unwrap();
            let acts = &state.remote_reports.by_remote[STUDIO].acts;
            (
                covered,
                acts.iter()
                    .map(|act| act.owing.contribution)
                    .collect::<Vec<_>>(),
            )
        };

        assert!(read(&store, there, covered, sidekick).await.is_empty());
        assert_eq!(
            state_of(covered).1,
            [
                RemoteContribution::Prompt(steered),
                RemoteContribution::Prompt(unread)
            ],
            "the Prompt held while the reading was on its way is not judged by it"
        );
        let covered = store.remote_reports_to_read(STUDIO, true).covered;
        assert!(read(&store, there, covered, sidekick).await.is_empty());
        assert_eq!(
            state_of(covered).1,
            [RemoteContribution::Prompt(steered)],
            "a reading begun after it was held, still not finding it, lets it go"
        );
        completes(&store, there, turn);
        writer.shutdown().await.unwrap();
    }

    /// Review item 4: an Answer whose answer never came back is this
    /// Sidekick's only where the Remote delivered that very act of this
    /// Peer's: one still being submitted owes nothing yet and is named in no
    /// lost Remote's Report, and one another act answered is let go of,
    /// telling nothing.
    #[tokio::test]
    async fn an_answer_is_this_sidekicks_only_as_its_own_act_delivered() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, turn) = working(&store, &workspace, "Run the auth suite.");
        let (asked, questionnaire) = asks(&store, there, turn);
        let ours = ActId::new();
        let covered = owe(
            &store,
            sidekick,
            there,
            RemoteContribution::Answer {
                questionnaire,
                act: ours,
            },
            false,
        );
        store
            .publish(
                there,
                vec![SessionChange::QuestionnaireAccepted { activity_id: asked }],
            )
            .unwrap();
        let told = read(&store, there, covered, sidekick).await;
        assert!(
            told.iter()
                .all(|report| !report.contains("settled its Turn")),
            "{told:?}"
        );
        store.remote_reports_lost(STUDIO, 1, SidekickOriginLoss::StoppedAnswering);
        assert_eq!(
            store.take_held_reports(sidekick).len(),
            0,
            "an Answer still being submitted owes nothing yet, so a lost Remote names nothing"
        );

        // Another act of this Peer's — another Sidekick's here — answers.
        settles(&store, there, asked, Some(of_this_peer(ActId::new())));
        completes(&store, there, turn);
        let covered = store.remote_reports_to_read(STUDIO, true).covered;
        assert_eq!(
            read(&store, there, covered, sidekick).await,
            Vec::<String>::new(),
            "the Turn another act answered is not this Sidekick's to hear of"
        );
        assert!(
            !store.is_remote_watched(STUDIO),
            "nothing waits on a read there"
        );
        writer.shutdown().await.unwrap();
    }

    /// Review item 8, and the second review's item 3: a following of a
    /// Remote under one Pairing ending ends only what was owed through that
    /// Pairing, and tells only of that — though the name was paired anew to
    /// the very same key, as the same Remote paired again is — and an act
    /// carried through a Pairing that ended after another came to be owed
    /// through is told lost at once.
    #[tokio::test]
    async fn a_lost_pairing_ends_only_what_was_owed_through_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, _) = working(&store, &workspace, "Run the auth suite.");
        let prompt = steered_by_this_peer(&store, there, "Fix the flaky login test.");
        let owing = |contribution| RemoteOwing {
            session_id: there,
            head: Some(there),
            title: Some("Run the auth suite.".to_owned()),
            contribution,
            confirmed: true,
        };
        // Owed through the Pairing made anew under the name, to the same
        // key, before the following of the one before let go.
        store.owe_remote_reports(
            sidekick,
            STUDIO,
            &paired(2),
            owing(RemoteContribution::Prompt(prompt)),
        );

        store.remote_reports_lost(STUDIO, 1, SidekickOriginLoss::Unpaired);
        assert_eq!(
            store.take_held_reports(sidekick).len(),
            0,
            "the Pairing that ended owed nothing to tell of"
        );
        assert!(
            store.is_remote_watched(STUDIO),
            "what is owed through the Pairing standing now is kept"
        );

        // An act whose answer came back through the Pairing that ended,
        // after one through the new Pairing was owed, is lost with its own.
        let late = steered_by_this_peer(&store, there, "Run it once more.");
        store.owe_remote_reports(
            sidekick,
            STUDIO,
            &paired(1),
            owing(RemoteContribution::Prompt(late)),
        );
        let told = store.take_held_reports(sidekick);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0]
                .to_string()
                .contains("the Pairing with the Remote \"studio\" ended"),
            "{}",
            told[0]
        );

        store.remote_reports_lost(STUDIO, 2, SidekickOriginLoss::Unpaired);
        assert_eq!(store.take_held_reports(sidekick).len(), 1);
        assert!(!store.is_remote_watched(STUDIO), "nothing is owed there");
        writer.shutdown().await.unwrap();
    }

    /// Review item 9: an Intervention asked and settled before any reading
    /// found it waiting is told once all the same, as settled; and of more
    /// than are told one by one in a reading, the rest are counted in one
    /// Report, each told once however often it is read.
    #[tokio::test]
    async fn every_intervention_is_told_once_settled_or_counted_beyond_the_bound() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, turn) = working(&store, &workspace, "Run the auth suite.");
        let steered = steered_by_this_peer(&store, there, "Fix the flaky login test.");
        store
            .deliver_steer(there, turn, steered)
            .unwrap()
            .expect("the working Turn takes the steer");
        let (first, _) = asks(&store, there, turn);
        answered(&store, there, first, None);
        for _ in 0..INTERVENTIONS_TOLD_AT_ONCE + 2 {
            asks(&store, there, turn);
        }
        let covered = owe(
            &store,
            sidekick,
            there,
            RemoteContribution::Prompt(steered),
            true,
        );

        let told = read(&store, there, covered, sidekick).await;
        assert_eq!(told.len(), INTERVENTIONS_TOLD_AT_ONCE + 1, "{told:?}");
        assert!(
            told[0].contains("asked a Questionnaire, which no longer waits on anyone"),
            "the one settled before it was read is told, as settled: {told:?}"
        );
        assert!(
            told[1].contains("asks a Questionnaire, which waits on an Answer"),
            "{told:?}"
        );
        assert!(
            told[INTERVENTIONS_TOLD_AT_ONCE]
                .contains("asked 3 more Questionnaires or Approvals than are told one by one"),
            "the rest are counted: {told:?}"
        );
        let covered = store.remote_reports_to_read(STUDIO, true).covered;
        assert_eq!(
            read(&store, there, covered, sidekick).await,
            Vec::<String>::new(),
            "and none is told again"
        );
        completes(&store, there, turn);
        writer.shutdown().await.unwrap();
    }

    /// Review item 4, the record of the acts a Sidekick has a hand in: an
    /// act on a Remote whose answer never came back stands confirmed only
    /// once a read shows what it left there as this Peer's — a Prompt by its
    /// identity, an Answer as that very act — and a read of its whole tree
    /// asked for after it, showing none of it, finds it never done. A
    /// listing, which shows the Session and nothing an act left in it,
    /// confirms none of it.
    #[tokio::test]
    async fn an_unknown_act_stands_confirmed_only_by_a_read_showing_what_it_left() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (answering, _) = working(&store, &workspace, "Answer for the user");
        let (prompting, _) = working(&store, &workspace, "Nudge the work");
        let (there, turn) = working(&store, &workspace, "Run the auth suite.");
        let (asked, questionnaire) = asks(&store, there, turn);
        let steered = steered_by_this_peer(&store, there, "Fix the flaky login test.");
        let unknown = |evidence| crate::sessions::RemoteAct {
            pairing: PAIRING.to_owned(),
            evidence: Some(evidence),
            ..crate::sessions::RemoteAct::default()
        };
        store.record_remote_sidekick_act(
            answering,
            STUDIO,
            there,
            unknown(RemoteContribution::Answer {
                questionnaire,
                act: ActId::new(),
            }),
        );
        store.record_remote_sidekick_act(
            prompting,
            STUDIO,
            there,
            unknown(RemoteContribution::Prompt(steered)),
        );
        let confirmed = |sidekick| {
            store
                .remote_sidekick_act(sidekick, STUDIO, there)
                .map(|act| act.confirmed)
        };

        let asked_at = store.moment();
        store.remote_listed(STUDIO, &paired(1), &store.list(None), asked_at);
        assert_eq!(
            (confirmed(answering), confirmed(prompting)),
            (Some(false), Some(false)),
            "a listing shows the Session, and nothing either act left there"
        );

        store
            .publish(
                there,
                vec![SessionChange::QuestionnaireAccepted { activity_id: asked }],
            )
            .unwrap();
        let outline = |store: &SessionStore| {
            let store = store.clone();
            async move { store.tree_outline(there).await.unwrap().unwrap() }
        };
        let asked_at = store.moment();
        store.judge_remote_acts(
            STUDIO,
            &paired(1),
            Some(OWN),
            &outline(&store).await.sessions,
            true,
            asked_at,
        );
        assert_eq!(
            (confirmed(answering), confirmed(prompting)),
            (Some(false), Some(true)),
            "the Prompt it left stands as this Peer's; whose submission is under way is not said"
        );

        // Another act of this Peer's — another Sidekick's — answers it.
        settles(&store, there, asked, Some(of_this_peer(ActId::new())));
        let asked_at = store.moment();
        store.judge_remote_acts(
            STUDIO,
            &paired(1),
            Some(OWN),
            &outline(&store).await.sessions,
            true,
            asked_at,
        );
        assert_eq!(
            confirmed(answering),
            None,
            "an Answer another act gave was never this one's: it is forgotten"
        );
        completes(&store, there, turn);
        writer.shutdown().await.unwrap();
    }

    /// Review item 10: however often a tree owed Reports is said to have
    /// moved, it is read once for all of it; and what moves while a reading
    /// is on its way is read again after it.
    #[tokio::test]
    async fn what_moves_in_a_tree_is_read_once_and_what_moves_during_a_reading_after_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, _) = working(&store, &workspace, "Run the auth suite.");
        let prompt = steered_by_this_peer(&store, there, "Fix the flaky login test.");
        store.owe_remote_reports(
            sidekick,
            STUDIO,
            &paired(1),
            RemoteOwing {
                session_id: there,
                head: Some(there),
                title: None,
                contribution: RemoteContribution::Prompt(prompt),
                confirmed: true,
            },
        );
        assert_eq!(store.remote_reports_to_read(STUDIO, false).trees, [there]);
        assert_eq!(
            store.remote_reports_to_read(STUDIO, false).trees,
            Vec::<SessionId>::new(),
            "nothing moved since it was read"
        );

        for _ in 0..50 {
            assert!(store.stir_remote_reports(STUDIO, there));
        }
        let reading = store.remote_reports_to_read(STUDIO, false);
        assert_eq!(reading.trees, [there], "fifty moves are one reading");
        // It moves again while that reading is on its way.
        assert!(store.stir_remote_reports(STUDIO, there));
        assert_eq!(
            store.remote_reports_to_read(STUDIO, false).trees,
            [there],
            "and is read again after it"
        );
        assert!(
            !store.stir_remote_reports(STUDIO, SessionId::new()),
            "a tree nothing is owed of is never read for it"
        );
        writer.shutdown().await.unwrap();
    }

    /// The second review's item 6: what is kept to tell a tree's Reports
    /// once — what was told of it, its Title — is kept only while something
    /// is owed in that tree, so a tree whose last act is spent is forgotten
    /// though another there stays owed.
    #[tokio::test]
    async fn a_tree_owed_nothing_more_is_forgotten_though_another_stays_owed() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (done, done_turn) = working(&store, &workspace, "Run the auth suite.");
        let (open, open_turn) = working(&store, &workspace, "Bump the deps.");
        for (there, turn) in [(done, done_turn), (open, open_turn)] {
            let (asked, questionnaire) = asks(&store, there, turn);
            let act = ActId::new();
            owe(
                &store,
                sidekick,
                there,
                RemoteContribution::Answer { questionnaire, act },
                true,
            );
            answered(&store, there, asked, Some(of_this_peer(act)));
        }
        let covered = store.remote_reports_to_read(STUDIO, true).covered;
        for there in [done, open] {
            assert!(read(&store, there, covered, sidekick).await.is_empty());
        }
        assert_eq!(store.remote_trees_kept(STUDIO), HashSet::from([done, open]));

        completes(&store, done, done_turn);
        let told = read(&store, done, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(
            store.remote_trees_kept(STUDIO),
            HashSet::from([open]),
            "the tree owed nothing more is forgotten, though another stays owed"
        );
        assert!(store.is_remote_watched(STUDIO));

        completes(&store, open, open_turn);
        assert_eq!(read(&store, open, covered, sidekick).await.len(), 1);
        assert!(store.remote_trees_kept(STUDIO).is_empty());
        assert!(!store.is_remote_watched(STUDIO));
        writer.shutdown().await.unwrap();
    }
    /// The Peer's Sidekick steers the settled Session `session_id` of the
    /// Remote, which is held as owed, and the Turn its Prompt begins leaves
    /// the Watch `watch` running as it settles: answers that Watch and the
    /// sequence a reading begun now covers.
    fn leaves_watching(
        store: &SessionStore,
        sidekick: SessionId,
        session_id: SessionId,
        watch: &str,
    ) -> (ProviderWatchId, u64) {
        let prompt_id = steered_by_this_peer(store, session_id, "Build the release.");
        let covered = owe(
            store,
            sidekick,
            session_id,
            RemoteContribution::Prompt(prompt_id),
            true,
        );
        let turn_id = store
            .deliver_prompt(session_id, prompt_id, None, DeliveredTurnStatus::Active)
            .unwrap()
            .expect("the Prompt begins a Turn")
            .turn_id;
        let watch_id = ProviderWatchId::new(watch.to_owned());
        store
            .start_watch(session_id, watch_id.clone(), watch.to_owned())
            .unwrap();
        completes(store, session_id, turn_id);
        (watch_id, covered)
    }

    fn agent() -> AgentIdentity {
        AgentIdentity {
            agent: AgentId::new("claude-agent"),
            selection: AgentSelection {
                provider: ProviderId::new("claude"),
                model: ModelId::new("claude-opus"),
                options: Vec::new(),
            },
        }
    }

    /// A Remote's Turn that left a Watch running is told as Monitoring, the
    /// Remote kept in view; the Continuation the Watch wakes is the
    /// Sidekick's, told as it settles — a reading in the moment between the
    /// Watch's settling and the Continuation's beginning telling nothing and
    /// letting go of nothing — and then nothing more is owed there.
    #[tokio::test]
    async fn a_remotes_continuation_a_watch_wakes_is_told_and_no_reading_between_ends_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, first) = working(&store, &workspace, "Run the auth suite.");
        completes(&store, there, first);
        let (build, covered) = leaves_watching(&store, sidekick, there, "cargo build --release");

        let told = read(&store, there, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("it left Watches running")
                && told[0].contains("\"cargo build --release\""),
            "{told:?}"
        );
        assert!(read(&store, there, covered, sidekick).await.is_empty());
        assert!(store.is_remote_watched(STUDIO), "the Watch runs on");

        store.settle_watch(there, &build, true);
        assert!(read(&store, there, covered, sidekick).await.is_empty());
        assert!(
            store.is_remote_watched(STUDIO),
            "the Continuation it woke may be yet to begin"
        );

        let continuation = store.begin_continuation(there, agent()).unwrap();
        assert!(read(&store, there, covered, sidekick).await.is_empty());
        completes(&store, there, continuation);
        let told = read(&store, there, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("has settled a Continuation")
                && !told[0].contains("left Watches running"),
            "{told:?}"
        );
        assert!(!store.is_remote_watched(STUDIO), "nothing more is owed");
        writer.shutdown().await.unwrap();
    }

    /// A Remote's Watches that end waking no one are told once, by a reading
    /// that finds them ended still once the wait for a Continuation is over;
    /// a Sidekick never told they ran is told nothing of their ending, nor
    /// is one that stopped them itself.
    #[tokio::test]
    async fn a_remotes_watches_ending_is_told_once_waited_on_to_a_sidekick_told_they_ran() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");

        let (there, first) = working(&store, &workspace, "Run the auth suite.");
        completes(&store, there, first);
        let (build, covered) = leaves_watching(&store, sidekick, there, "cargo build");
        assert_eq!(read(&store, there, covered, sidekick).await.len(), 1);
        store.settle_watch(there, &build, false);
        for _ in 0..2 {
            assert!(read(&store, there, covered, sidekick).await.is_empty());
            assert!(store.is_remote_watched(STUDIO), "a Continuation may come");
        }
        let told = read_waiting(&store, there, covered, sidekick, Duration::ZERO).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("ended without waking its Agent")
                && told[0].contains("origin \"studio\""),
            "{told:?}"
        );
        assert!(!store.is_remote_watched(STUDIO));

        // Read first only once the Watch had ended: its Turn's settling is
        // all there is to tell.
        let (unseen, first) = working(&store, &workspace, "Bump the deps.");
        completes(&store, unseen, first);
        let (build, covered) = leaves_watching(&store, sidekick, unseen, "cargo build");
        store.settle_watch(unseen, &build, false);
        let told = read(&store, unseen, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(!told[0].contains("left Watches running"), "{told:?}");
        assert!(read(&store, unseen, covered, sidekick).await.is_empty());
        assert!(!store.is_remote_watched(STUDIO));

        // Stopped by the Sidekick itself.
        let (stopped, first) = working(&store, &workspace, "Ship the docs.");
        completes(&store, stopped, first);
        let (build, covered) = leaves_watching(&store, sidekick, stopped, "cargo doc");
        assert_eq!(read(&store, stopped, covered, sidekick).await.len(), 1);
        store.remote_sidekick_stopped_watches(STUDIO, sidekick, stopped);
        store.settle_watch(stopped, &build, false);
        assert!(
            read_waiting(&store, stopped, covered, sidekick, Duration::ZERO)
                .await
                .is_empty()
        );

        // Watches its work leaves there afterwards are none it stopped,
        // though no reading came between.
        let (doc, covered) = leaves_watching(&store, sidekick, stopped, "cargo doc");
        assert_eq!(read(&store, stopped, covered, sidekick).await.len(), 1);
        store.remote_sidekick_stopped_watches(STUDIO, sidekick, stopped);
        store.settle_watch(stopped, &doc, false);
        let (tests, covered) = leaves_watching(&store, sidekick, stopped, "cargo test");
        let told = read(&store, stopped, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("\"cargo test\""), "{told:?}");
        store.settle_watch(stopped, &tests, false);
        let told = read_waiting(&store, stopped, covered, sidekick, Duration::ZERO).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("ended without waking its Agent"),
            "{told:?}"
        );
        assert!(!store.is_remote_watched(STUDIO));
        writer.shutdown().await.unwrap();
    }

    /// A Remote's Watch running on beneath a Turn someone else began is the
    /// Sidekick's still; once it has ended there, with that Turn between, the
    /// Sidekick is told so once, and a Continuation after it is not its own.
    #[tokio::test]
    async fn a_remotes_watch_ended_past_a_turn_someone_else_began_is_told_as_within_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");
        let (there, first) = working(&store, &workspace, "Run the auth suite.");
        completes(&store, there, first);
        let (build, covered) = leaves_watching(&store, sidekick, there, "cargo build");
        assert_eq!(read(&store, there, covered, sidekick).await.len(), 1);

        let StoreOutcome::Created(admission) = store
            .admit(
                there,
                AdmitPromptRequest {
                    prompt: asking("While you wait, tidy the changelog."),
                    delivery: PromptDelivery::Steer,
                },
                Vec::new(),
                None,
            )
            .unwrap()
        else {
            panic!("the Prompt is admitted afresh");
        };
        let theirs = store
            .deliver_prompt(
                there,
                admission.prompt.id,
                None,
                DeliveredTurnStatus::Active,
            )
            .unwrap()
            .expect("the Prompt begins a Turn")
            .turn_id;
        assert!(
            read(&store, there, covered, sidekick).await.is_empty(),
            "the Watch runs on beneath their Turn"
        );
        assert!(store.is_remote_watched(STUDIO));

        store.settle_watch(there, &build, true);
        let told = read(&store, there, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("settled within a Turn someone else began"),
            "{told:?}"
        );
        completes(&store, there, theirs);
        let continuation = store.begin_continuation(there, agent()).unwrap();
        completes(&store, there, continuation);
        assert!(read(&store, there, covered, sidekick).await.is_empty());
        assert!(!store.is_remote_watched(STUDIO));
        writer.shutdown().await.unwrap();
    }
    /// A Remote's Continuation read of only once it had settled, the Watch
    /// that woke it never found live, is the Sidekick's by the Watch Outcome
    /// standing in it; one with nothing to show a Watch ran on is not.
    #[tokio::test]
    async fn a_remotes_continuation_first_read_settled_is_the_sidekicks_by_its_watch_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick, _) = working(&store, &workspace, "Plan the work");

        let (there, first) = working(&store, &workspace, "Run the auth suite.");
        completes(&store, there, first);
        let (build, covered) = leaves_watching(&store, sidekick, there, "cargo build");
        store.settle_watch(there, &build, true);
        let continuation = store.begin_continuation(there, agent()).unwrap();
        store
            .publish(
                there,
                vec![SessionChange::ActivityAdded {
                    activity: Activity::WatchOutcome {
                        id: ActivityId::new(),
                        turn_id: continuation,
                        status: WatchOutcomeStatus::Completed,
                        description: "cargo build".to_owned(),
                        summary: None,
                    },
                }],
            )
            .unwrap();
        completes(&store, there, continuation);
        let told = read(&store, there, covered, sidekick).await;
        assert_eq!(told.len(), 2, "{told:?}");
        assert!(told[0].contains("has settled its Turn"), "{told:?}");
        assert!(told[1].contains("has settled a Continuation"), "{told:?}");
        assert!(!store.is_remote_watched(STUDIO));

        let (plain, first) = working(&store, &workspace, "Bump the deps.");
        completes(&store, plain, first);
        let (build, covered) = leaves_watching(&store, sidekick, plain, "cargo build");
        store.settle_watch(plain, &build, false);
        let continuation = store.begin_continuation(plain, agent()).unwrap();
        completes(&store, plain, continuation);
        let told = read(&store, plain, covered, sidekick).await;
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("has settled its Turn"), "{told:?}");
        assert!(!store.is_remote_watched(STUDIO));
        writer.shutdown().await.unwrap();
    }
}
