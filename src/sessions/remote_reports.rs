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
//! really took it, each Answer's delivery, each Intervention's asking, each
//! Turn's settling. Read so, what happened before this Server first read the
//! tree, or while it had lost its place, is followed exactly as what it
//! hears of at once; and whatever the replay finds owed that was told before
//! is not told again. Whether each Session of the tree works is the
//! outline's to say, at that one moment, so a branch is let go of only once
//! the Remote says nothing in it works.
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

use std::collections::{HashMap, HashSet};

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
        Owed, SidekickWork, WorkTree, answer_delivered, intervention_asked,
        let_go_of_settled_branches, let_go_of_untaken_prompts, take_prompt, turns_settled,
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

/// What a Sidekick asked of a Remote's Session that it is owed Reports of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteContribution {
    /// A Prompt it sent, the first of a Session it began among them.
    Prompt(PromptId),
    /// An Answer it gave a Questionnaire there, as the act `act`.
    Answer {
        questionnaire: QuestionnaireId,
        act: ActId,
    },
}

/// The work Sidekicks set going in Remotes' Sessions, by the Remote's name.
#[derive(Default)]
pub(super) struct RemoteReports {
    by_remote: HashMap<String, OwedThere>,
}

/// What Sidekicks are owed of one Remote's Sessions.
struct OwedThere {
    /// The key fingerprint of the Pairing all of it was carried through.
    pairing: String,
    acts: Vec<OwedAct>,
    /// The sequence the next act held takes.
    next_seq: u64,
    /// Everything a replay found owed that has been told, so none is told
    /// twice: each Turn's settling and each Intervention, to each Sidekick.
    told: HashSet<Owed>,
    /// The Title the last reading of each tree acted in gave the Session
    /// heading it, by that Session.
    titles: HashMap<SessionId, String>,
    /// The Sessions acted on there something moved in since they were last
    /// read.
    stirred: HashSet<SessionId>,
}

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

/// What one reading of a Remote's tree found: what is owed and not yet told,
/// in the order it happened, and the acts it spent, let go of once that is
/// told.
#[derive(Clone, Debug, Default)]
pub(crate) struct RemoteFollowing {
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
    /// the Remote `remote` carried through the Pairing whose key fingerprint
    /// is `pairing`, as owed Reports of — owing nothing until a read finds it
    /// done, where the Remote's answer to it never came back. Nothing is held
    /// for a Sidekick whose Session is no longer held.
    pub(crate) fn owe_remote_reports(
        &self,
        sidekick: SessionId,
        remote: &str,
        pairing: &str,
        owing: RemoteOwing,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if !state.sessions.contains_key(&sidekick) {
            return;
        }
        // What was owed through another Pairing is that Pairing's, which no
        // longer stands as it did.
        state.keep_remote_reports_pairing(remote, pairing);
        let owed = state
            .remote_reports
            .by_remote
            .entry(remote.to_owned())
            .or_insert_with(|| OwedThere {
                pairing: pairing.to_owned(),
                acts: Vec::new(),
                next_seq: 1,
                told: HashSet::new(),
                titles: HashMap::new(),
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
        let stirred = owed
            .acts
            .iter()
            .any(|act| act.owing.head.unwrap_or(act.owing.session_id) == head);
        if stirred {
            owed.stirred.insert(head);
        }
        stirred
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
            let tree = act.owing.head.unwrap_or(act.owing.session_id);
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
    /// `read_by` belongs to, as a reading through the Pairing whose key
    /// fingerprint is `pairing` gave it — this Server known there by the key
    /// fingerprint `own` — the acts up to `covered` held before it was asked:
    /// replays the acts made in it through the Sidekick Report rule, and
    /// answers what that finds owed and not yet told, in the order it
    /// happened there, and the acts it spent. Each act it shows was never
    /// done, of those it covers, is let go of.
    pub(crate) fn follow_remote_outline(
        &self,
        remote: &str,
        pairing: &str,
        own: &str,
        covered: u64,
        read_by: SessionId,
        outline: &SessionTreeOutline,
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
        if owed.pairing != pairing {
            return RemoteFollowing::default();
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
        let mut replay = Replay {
            snapshots: &snapshots,
            works: HashMap::new(),
            held: &held,
            liveness_known: false,
        };
        let found = replay.run(&acts, own);
        let remaining = replay.remaining();
        // An act whose Sidekick is owed nothing more of this tree is spent,
        // unless what it waits on may yet come: a Prompt still waiting, or an
        // Answer still being submitted.
        let spent = owed
            .acts
            .iter()
            .filter(|act| {
                act.owing.head == Some(head_id)
                    && act.seq <= covered
                    && !remaining
                        .iter()
                        .any(|(_, work)| work.sidekick == act.sidekick)
                    && evidence(&snapshots, act, own) != Evidence::Pending
            })
            .map(|act| act.seq)
            .collect();
        owed.titles.insert(head_id, title.clone());
        RemoteFollowing {
            raises: owed.untold(found, &snapshots, remote, head_id, &title),
            spent,
        }
    }

    /// Tells each of `raises` — found owed by a reading of the Remote
    /// `remote` through the Pairing whose key fingerprint is `pairing`, and
    /// put in words, each with what it tells — to its Sidekick, held for its
    /// Agent as a Report of this Server's own Sessions is, unless it was told
    /// meanwhile or that Pairing no longer stands as it did; and lets go of
    /// the acts that reading `spent`, owed nothing more once that is told.
    pub(crate) fn tell_remote_reports(
        &self,
        remote: &str,
        pairing: &str,
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
        if owed.pairing != pairing {
            return;
        }
        owed.acts.retain(|act| !spent.contains(&act.seq));
        let mut reports = Vec::new();
        for (sidekick, report, told) in raises {
            if told.iter().all(|told| owed.told.contains(told)) {
                continue;
            }
            owed.told.extend(told);
            reports.push((sidekick, report));
        }
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
        owed.titles.remove(&read_by);
        if owed.acts.is_empty() {
            state.remote_reports.by_remote.remove(remote);
        }
    }

    /// Takes up that the Remote `remote`, followed through the Pairing whose
    /// key fingerprint is `pairing` — `None` where none stands by that name —
    /// stopped answering, or that Pairing ended, for `loss`: each Sidekick
    /// owed Reports through it is told so once, naming the Sessions it was
    /// waiting on, and everything owed through it ends. What was owed
    /// through another Pairing is that Pairing's, and is left. An act whose
    /// answer never came back, which owes nothing yet, waits still for a read
    /// to tell where the Remote only stopped answering; a Pairing ended takes
    /// it too.
    pub(crate) fn remote_reports_lost(
        &self,
        remote: &str,
        pairing: Option<&str>,
        loss: SidekickOriginLoss,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(pairing) = pairing
            && state
                .remote_reports
                .by_remote
                .get(remote)
                .is_some_and(|owed| owed.pairing != pairing)
        {
            return;
        }
        state.lose_remote_reports(remote, loss);
    }
}

impl OwedThere {
    /// What of `found` — owed, in the order it happened in the tree headed
    /// by `head`, titled `title`, of the Remote `remote`, whose Sessions are
    /// `snapshots` — was not told before, as what tells it: Interventions
    /// one by one up to so many for each Sidekick, any more counted in one
    /// Report, and each Turn's settling to be put in words.
    fn untold(
        &self,
        found: Vec<Owed>,
        snapshots: &HashMap<SessionId, &SessionSnapshot>,
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
            let session = act.owing.head.unwrap_or(act.owing.session_id);
            let title = owed
                .titles
                .get(&session)
                .cloned()
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
                owed.titles.clear();
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
        owed.titles.remove(&session_id);
        if owed.acts.is_empty() {
            self.remote_reports.by_remote.remove(remote);
        }
    }

    /// Drops everything owed at Remotes to a Sidekick whose Session is among
    /// `deleted`: there is no Agent left to tell.
    pub(super) fn forget_remote_sidekicks(&mut self, deleted: &[SessionId]) {
        self.remote_reports.by_remote.retain(|_, owed| {
            owed.acts.retain(|act| !deleted.contains(&act.sidekick));
            !owed.acts.is_empty()
        });
    }

    /// The Pairing with the Remote `remote` stands now with the key
    /// fingerprint `pairing`: whatever was owed there through another is
    /// lost with it.
    pub(super) fn keep_remote_reports_pairing(&mut self, remote: &str, pairing: &str) {
        if self
            .remote_reports
            .by_remote
            .get(remote)
            .is_some_and(|owed| owed.pairing != pairing)
        {
            self.lose_remote_reports(remote, SidekickOriginLoss::Unpaired);
        }
    }
}

/// What an outline shows of whether an act was done.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Evidence {
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
    let Some(snapshot) = snapshots.get(&act.owing.session_id) else {
        return Evidence::Absent;
    };
    match act.owing.contribution {
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
    },
    Settled {
        session_id: SessionId,
        turn_id: TurnId,
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
        for happened in self.happened(acts, own) {
            match happened {
                Happened::Taken {
                    session_id,
                    prompt_id,
                    turn_id,
                } => take_prompt(self, session_id, prompt_id, turn_id),
                Happened::Answered {
                    session_id,
                    sidekick,
                    turn_id,
                } => answer_delivered(self, session_id, sidekick, turn_id),
                Happened::Asked {
                    session_id,
                    turn_id,
                    activity_id,
                    intervention,
                } => found.extend(intervention_asked(
                    self,
                    session_id,
                    turn_id,
                    activity_id,
                    intervention,
                )),
                Happened::Settled {
                    session_id,
                    turn_id,
                } => found.extend(turns_settled(self, session_id, &[turn_id])),
            }
        }
        self.liveness_known = true;
        let sessions = self.snapshots.keys().copied().collect::<Vec<_>>();
        for session_id in sessions {
            let_go_of_untaken_prompts(self, session_id);
            let_go_of_settled_branches(self, session_id);
        }
        found
    }

    /// Everything the outline says happened that the rule takes up, in the
    /// order the Remote's one clock stamped it: a taking, a delivery, an
    /// asking and a settling stamped in one moment in that order, as one
    /// commit there would have them.
    fn happened(&self, acts: &[OwedAct], own: &str) -> Vec<Happened> {
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
                            2,
                            Happened::Asked {
                                session_id,
                                turn_id: *turn_id,
                                activity_id: *id,
                                intervention: SidekickIntervention::Questionnaire,
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
                                    1,
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
                        2,
                        Happened::Asked {
                            session_id,
                            turn_id: *turn_id,
                            activity_id: *id,
                            intervention: SidekickIntervention::Approval,
                        },
                    )),
                    _ => {}
                }
            }
            for turn in &snapshot.turns {
                if turn.status.is_terminal() {
                    happened.push((
                        at(turn.settled_at),
                        3,
                        Happened::Settled {
                            session_id,
                            turn_id: turn.id,
                        },
                    ));
                }
            }
        }
        happened.sort_by_key(|(stamp, order, _)| (*stamp, *order));
        happened
            .into_iter()
            .map(|(_, _, happened)| happened)
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
        AdmitPromptRequest, Answer, CreateSessionRequest, ExecutionDirectory, InitialPrompt,
        PromptDelivery, Questionnaire, SessionChange,
    };
    use crate::questionnaire::Question;
    use crate::sessions::{
        DeliveredTurnStatus, ProviderTurnOutcome, StoreOutcome, TrailingCommandOutput,
    };
    use crate::storage::{RestoredSessions, StorageRepository, StorageWriter};

    /// The Remote every act here was carried to, the Pairing it was carried
    /// through, and this Server as the Remote knows it.
    const STUDIO: &str = "studio";
    const PAIRING: &str = "SHA256:studio";
    const OWN: &str = "own-key";

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
        let outline = store
            .tree_outline(session_id)
            .await
            .unwrap()
            .expect("the tree is held");
        let following =
            store.follow_remote_outline(STUDIO, PAIRING, OWN, covered, session_id, &outline);
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
                    owed,
                } => {
                    let snapshot = store.snapshot(session_id)?;
                    let turn = snapshot.turns.iter().find(|turn| turn.id == turn_id)?;
                    let report = crate::sessions::settled_report(subject, &snapshot, turn)?;
                    Some((sidekick, report, vec![owed]))
                }
            })
            .collect();
        store.tell_remote_reports(STUDIO, PAIRING, told, &following.spent);
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
            PAIRING,
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
        let mut state_of = |covered| {
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
}
