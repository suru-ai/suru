//! Sidekick Reports owed of a Remote's Sessions (CONTEXT.md: Sidekick
//! Report; ADR 0044).
//!
//! A Remote knows nothing of a Peer's Sidekick: what a Sidekick carries there
//! stands as a Sidekick's on that Peer, and nothing on the Remote owes it a
//! Report. So the Sidekick's own Server holds what the Sidekick is owed there,
//! as it holds what it is owed of its own Sessions (see `sidekick_reports`),
//! piece by piece and by the same rule of lifetime: a Prompt it sent — the
//! first of a Session it began among them — until a Turn takes it, and that
//! Turn until it settles; an Answer it gave, once delivered to the Agent, and
//! the Turn it went on in until that Turn settles; and the Subagents either
//! Turn set working, until the whole branch it set going has settled. Each
//! such Turn is told once as it settles, and each Questionnaire or Approval
//! it, or a Subagent of its branch, comes to owe is told once.
//!
//! What a Remote's Session is doing is the Remote's to say, so this Server
//! follows it as far as the Remote lets it: it keeps the Remote in view while
//! anything is owed there (see `crate::server::operations::remote_entries`),
//! and whenever it begins following the Remote, loses its place, or hears
//! that something moved in a Session owed, it reads that Session afresh
//! through the Pairing and takes up what the read says (see
//! [`SessionStore::follow_remote_reports`]). So a Turn that settled before
//! this Server began following, or while it had lost its place, is told from
//! the read that finds it settled — and only once, since what is owed moves
//! on as it is told. Read after the fact, a Prompt delivered into a Turn is
//! taken to be that Turn's, as this Server cannot tell a steer the working
//! Turn took from one only recorded as it settled; and an Intervention
//! settled before a read finds it waiting is never told, since it no longer
//! waits on anyone.
//!
//! An act whose answer never came back owes nothing until a read finds it
//! done — its Prompt standing in the Session, its Answer delivered — and then
//! owes what a confirmed act would; one a read finds never done is let go.
//! Until then the Remote is kept in view so that a read can tell.
//!
//! A Remote that stops answering while Reports are owed from it, or whose
//! Pairing ends, is told to each Sidekick owed them once, naming the
//! Sessions it was waiting on there, and everything owed there ends with it:
//! what comes of those Sessions afterwards the Sidekick learns by reading
//! them once the Remote answers again. Nothing here outlives a Server stop,
//! and nothing reaches a Transcript.

use std::collections::{HashMap, HashSet};

use crate::protocol::{
    Activity, ActivityStatus, ApprovalId, ApprovalOutcome, MessageRole, Outlook, Prompt, PromptId,
    PromptStatus, QuestionnaireId, QuestionnaireOutcome, SessionId, SessionReference,
    SessionSnapshot, SnapshotWithSummary, TurnId,
};
use crate::provider::{
    SidekickIntervention, SidekickOriginLoss, SidekickReport, SidekickReportSubject,
};

use super::{
    SessionStore, SessionStoreState,
    sidekick_reports::{settled_report, spawning_turn},
};

/// The most Interventions told of one piece of work, however many the Remote
/// says its Sessions come to owe.
const TOLD_INTERVENTIONS: usize = 64;

/// What a Sidekick asked of a Remote's Session that it is owed Reports of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteContribution {
    /// A Prompt it sent, the first of a Session it began among them.
    Prompt(PromptId),
    /// An Answer it gave a Questionnaire there.
    Answer(QuestionnaireId),
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
    works: Vec<RemoteWork>,
    /// The Sessions acted on there something moved in since they were last
    /// read.
    stirred: HashSet<SessionId>,
}

/// One piece of work a Sidekick set going in a Remote's Session, of which it
/// is owed Reports for as long as it lasts.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RemoteWork {
    /// The Sidekick's own Session.
    sidekick: SessionId,
    /// The Session it went to there: a top-level Session, or a Subagent's.
    session_id: SessionId,
    /// The top-level Session heading it there, once known.
    head: Option<SessionId>,
    /// That Session's Title, as last read.
    title: Option<String>,
    stage: RemoteStage,
    /// Whether it is known to have been done: the Remote answered the act, or
    /// a read found it done.
    confirmed: bool,
    /// The Interventions already told of it.
    told: Vec<Told>,
}

/// How far a piece of a Sidekick's work at a Remote has gone, as far as this
/// Server has read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteStage {
    /// A Prompt it sent, which no Turn has taken yet.
    Sent(PromptId),
    /// An Answer it gave, not yet delivered to the Agent.
    Answered(QuestionnaireId),
    /// A Turn that took a Prompt it sent, or that an Answer it gave went on
    /// in, still working.
    Working(TurnId),
    /// Such a Turn, settled and told, whose Subagents work on.
    Delegated(TurnId),
}

/// An Intervention told to a Sidekick, so it is told once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Told {
    Questionnaire(QuestionnaireId),
    Approval(ApprovalId),
}

impl RemoteReports {
    /// Whether anything is owed of a Session of the Remote `remote`.
    pub(super) fn owes_at(&self, remote: &str) -> bool {
        self.by_remote.contains_key(remote)
    }

    /// The top-level Sessions of the Remote `remote` heading what is owed
    /// there, where known.
    pub(super) fn heads_at(&self, remote: &str) -> HashSet<SessionId> {
        self.by_remote
            .get(remote)
            .into_iter()
            .flat_map(|owed| owed.works.iter().filter_map(|work| work.head))
            .collect()
    }
}

impl SessionStore {
    /// Holds `contribution`, which the Sidekick of `sidekick` made to the
    /// Session `session_id` of the Remote `remote` through the Pairing whose
    /// key fingerprint is `pairing` — headed there by `head`, where known —
    /// as owed Reports of: `confirmed` where the Remote answered the act,
    /// and otherwise owing nothing until a read finds it done. Nothing is
    /// held for a Sidekick whose Session is no longer held.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn owe_remote_reports(
        &self,
        sidekick: SessionId,
        remote: &str,
        pairing: &str,
        session_id: SessionId,
        head: Option<SessionId>,
        contribution: RemoteContribution,
        confirmed: bool,
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
        if state
            .remote_reports
            .by_remote
            .get(remote)
            .is_some_and(|owed| owed.pairing != pairing)
        {
            state.lose_remote_reports(remote, SidekickOriginLoss::Unpaired);
        }
        let owed = state
            .remote_reports
            .by_remote
            .entry(remote.to_owned())
            .or_insert_with(|| OwedThere {
                pairing: pairing.to_owned(),
                works: Vec::new(),
                stirred: HashSet::new(),
            });
        let stage = match contribution {
            RemoteContribution::Prompt(prompt_id) => RemoteStage::Sent(prompt_id),
            RemoteContribution::Answer(questionnaire_id) => RemoteStage::Answered(questionnaire_id),
        };
        owed.stirred.insert(session_id);
        // Asked again, the same act is the same piece of work.
        if let Some(held) = owed.works.iter_mut().find(|work| {
            work.sidekick == sidekick && work.session_id == session_id && work.stage == stage
        }) {
            held.confirmed |= confirmed;
            held.head = held.head.or(head);
            return;
        }
        owed.works.push(RemoteWork {
            sidekick,
            session_id,
            head,
            title: None,
            stage,
            confirmed,
            told: Vec::new(),
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
            .works
            .iter()
            .filter(|work| work.head.unwrap_or(work.session_id) == head)
            .map(|work| work.session_id)
            .collect::<Vec<_>>();
        let any = !stirred.is_empty();
        owed.stirred.extend(stirred);
        any
    }

    /// The Sessions of the Remote `remote` to read for what is owed of them —
    /// every one, where `all`, and otherwise those something moved in since
    /// they were last read — each with the Session heading it there, where
    /// known.
    pub(crate) fn remote_reports_to_read(
        &self,
        remote: &str,
        all: bool,
    ) -> Vec<(SessionId, Option<SessionId>)> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return Vec::new();
        };
        let stirred = std::mem::take(&mut owed.stirred);
        let mut read = Vec::<(SessionId, Option<SessionId>)>::new();
        for work in &owed.works {
            if (all || stirred.contains(&work.session_id))
                && !read.iter().any(|(held, _)| *held == work.session_id)
            {
                read.push((work.session_id, work.head));
            }
        }
        read
    }

    /// Takes up `read`, a Session of the Remote `remote` as a read of it
    /// through the Pairing whose key fingerprint is `pairing` found it,
    /// headed there by `head`: each piece of work owed of it goes as far as
    /// the read says, and each Turn settled or Intervention asked that it
    /// owes a Sidekick is told, held for that Sidekick's Agent as a Report
    /// of its own Server's Sessions is.
    pub(crate) fn follow_remote_reports(
        &self,
        remote: &str,
        pairing: &str,
        read: &SnapshotWithSummary,
        head: SessionId,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let snapshot = &read.snapshot;
        let session_id = snapshot.session.id;
        let title = if head == session_id {
            Some(snapshot.title.clone())
        } else {
            state.remote_session_title(remote, head)
        };
        // Whether a Subagent works on beneath its row is said by the tree
        // its Session heads there, where that tree is followed; otherwise its
        // row alone says.
        let working = state
            .remote_tree(remote, head)
            .map(|tree| {
                tree.subagents
                    .iter()
                    .filter(|entry| entry.working_since.is_some())
                    .map(|entry| entry.session_id)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let Some(owed) = state.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        if owed.pairing != pairing {
            return;
        }
        let reading = Reading {
            remote,
            head,
            snapshot,
            working: &working,
        };
        let mut reports = Vec::new();
        owed.works.retain_mut(|work| {
            if work.session_id != session_id {
                return true;
            }
            work.head = Some(head);
            if let Some(title) = &title {
                work.title = Some(title.clone());
            }
            reading.advance(work, &mut reports)
        });
        if owed.works.is_empty() {
            state.remote_reports.by_remote.remove(remote);
        }
        for (sidekick, report) in reports {
            state.hold_report(sidekick, report);
        }
    }

    /// Lets go of everything owed of the Session `session_id` of the Remote
    /// `remote`, which a read there found it does not hold, or cannot read:
    /// there is nothing more to follow, and nothing to tell.
    pub(crate) fn let_go_of_remote_reports(&self, remote: &str, session_id: SessionId) {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .let_go_of_remote_session_reports(remote, session_id);
    }

    /// Takes up that the Remote `remote` stopped answering, or its Pairing
    /// ended, for `loss`: each Sidekick owed Reports there is told so once,
    /// naming the Sessions it was waiting on, and everything owed there ends.
    /// An act whose answer never came back, which owes nothing yet, waits
    /// still for a read to tell where the Remote only stopped answering; a
    /// Pairing ended takes it too.
    pub(crate) fn remote_reports_lost(&self, remote: &str, loss: SidekickOriginLoss) {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .lose_remote_reports(remote, loss);
    }
}

impl SessionStoreState {
    /// See [`SessionStore::remote_reports_lost`].
    pub(super) fn lose_remote_reports(&mut self, remote: &str, loss: SidekickOriginLoss) {
        let Some(mut owed) = self.remote_reports.by_remote.remove(remote) else {
            return;
        };
        let mut told = Vec::<(SessionId, Vec<(SessionId, String)>)>::new();
        for work in owed.works.iter().filter(|work| work.confirmed) {
            let session = work.head.unwrap_or(work.session_id);
            let title = work
                .title
                .clone()
                .or_else(|| self.remote_session_title(remote, session))
                .unwrap_or_default();
            let at = match told
                .iter()
                .position(|(sidekick, _)| *sidekick == work.sidekick)
            {
                Some(at) => at,
                None => {
                    told.push((work.sidekick, Vec::new()));
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
            owed.works.retain(|work| !work.confirmed);
            if !owed.works.is_empty() {
                owed.stirred.clear();
                self.remote_reports
                    .by_remote
                    .insert(remote.to_owned(), owed);
            }
        }
    }

    /// See [`SessionStore::let_go_of_remote_reports`]: everything owed of
    /// the Session `session_id` of the Remote `remote`, or beneath it there.
    pub(super) fn let_go_of_remote_session_reports(&mut self, remote: &str, session_id: SessionId) {
        let Some(owed) = self.remote_reports.by_remote.get_mut(remote) else {
            return;
        };
        owed.works
            .retain(|work| work.session_id != session_id && work.head != Some(session_id));
        if owed.works.is_empty() {
            self.remote_reports.by_remote.remove(remote);
        }
    }

    /// Drops everything owed at Remotes to a Sidekick whose Session is among
    /// `deleted`: there is no Agent left to tell.
    pub(super) fn forget_remote_sidekicks(&mut self, deleted: &[SessionId]) {
        self.remote_reports.by_remote.retain(|_, owed| {
            owed.works.retain(|work| !deleted.contains(&work.sidekick));
            !owed.works.is_empty()
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

/// One read of a Remote's Session, as what is owed of it is taken up from it.
struct Reading<'a> {
    remote: &'a str,
    /// The top-level Session heading it there.
    head: SessionId,
    snapshot: &'a SessionSnapshot,
    /// The Subagents beneath its head the Remote says work now.
    working: &'a HashSet<SessionId>,
}

impl Reading<'_> {
    /// Moves `work` on as far as the read says, telling in `reports` what it
    /// owes its Sidekick on the way: answers whether anything of it is owed
    /// still.
    fn advance(
        &self,
        work: &mut RemoteWork,
        reports: &mut Vec<(SessionId, SidekickReport)>,
    ) -> bool {
        let snapshot = self.snapshot;
        loop {
            match work.stage {
                RemoteStage::Sent(prompt_id) => {
                    let Some(prompt) = snapshot
                        .prompts
                        .iter()
                        .find(|prompt| prompt.id == prompt_id)
                    else {
                        // Never admitted there, or nothing of it is left to
                        // follow.
                        return false;
                    };
                    work.confirmed = true;
                    match prompt.status {
                        PromptStatus::Pending => return true,
                        PromptStatus::Failed | PromptStatus::Cancelled => return false,
                        PromptStatus::Delivered => match taking_turn(snapshot, prompt) {
                            Some(turn_id) => work.stage = RemoteStage::Working(turn_id),
                            None => return false,
                        },
                    }
                }
                RemoteStage::Answered(questionnaire_id) => {
                    let asked = snapshot
                        .activities
                        .iter()
                        .find_map(|activity| match activity {
                            Activity::Questionnaire {
                                turn_id,
                                questionnaire,
                                outcome,
                                answer,
                                author,
                                ..
                            } if questionnaire.id == questionnaire_id => {
                                Some((*turn_id, *outcome, answer.is_some() && author.is_some()))
                            }
                            _ => None,
                        });
                    match asked {
                        Some((_, QuestionnaireOutcome::Submitting, _)) => {
                            work.confirmed = true;
                            return true;
                        }
                        // A Sidekick's Answer, delivered to the Agent.
                        Some((turn_id, QuestionnaireOutcome::Answered, true)) => {
                            work.confirmed = true;
                            work.stage = RemoteStage::Working(turn_id);
                        }
                        _ => return false,
                    }
                }
                RemoteStage::Working(turn_id) => {
                    let Some(turn) = snapshot.turns.iter().find(|turn| turn.id == turn_id) else {
                        return false;
                    };
                    self.tell_interventions(work, turn_id, reports);
                    if !turn.status.is_terminal() {
                        return true;
                    }
                    if let Some(report) =
                        settled_report(self.subject(work, self.own_subagent()), snapshot, turn)
                    {
                        reports.push((work.sidekick, report));
                    }
                    if !self.branch_works_on(turn_id) {
                        return false;
                    }
                    work.stage = RemoteStage::Delegated(turn_id);
                    return true;
                }
                RemoteStage::Delegated(turn_id) => {
                    if !self.branch_works_on(turn_id) {
                        return false;
                    }
                    self.tell_interventions(work, turn_id, reports);
                    return true;
                }
            }
        }
    }

    /// The Session read, as the Subagent's Session a Report names beneath
    /// its head, where it is one.
    fn own_subagent(&self) -> Option<SessionId> {
        let session_id = self.snapshot.session.id;
        (session_id != self.head).then_some(session_id)
    }

    /// What a Report of `work` is about: its head, by its Title, at the
    /// Remote, and `subagent` beneath it where what it tells of happened in
    /// one.
    fn subject(&self, work: &RemoteWork, subagent: Option<SessionId>) -> SidekickReportSubject {
        SidekickReportSubject {
            session: SessionReference::new(Outlook::Remote(self.remote.to_owned()), self.head),
            title: work
                .title
                .clone()
                .unwrap_or_else(|| self.snapshot.title.clone()),
            subagent,
        }
    }

    /// Tells `work`'s Sidekick, once each, of every Questionnaire and
    /// Approval the read finds waiting in the Turn `turn_id` — and beneath
    /// each Subagent whose latest row stands in that Turn — and has not told
    /// it of yet, so many at most.
    fn tell_interventions(
        &self,
        work: &mut RemoteWork,
        turn_id: TurnId,
        reports: &mut Vec<(SessionId, SidekickReport)>,
    ) {
        let snapshot = self.snapshot;
        let own = snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Questionnaire {
                    turn_id: asked_in,
                    questionnaire,
                    outcome: QuestionnaireOutcome::Pending,
                    ..
                } if *asked_in == turn_id => Some((
                    Told::Questionnaire(questionnaire.id),
                    SidekickIntervention::Questionnaire,
                    self.own_subagent(),
                )),
                Activity::Approval {
                    turn_id: asked_in,
                    approval,
                    outcome: ApprovalOutcome::Pending,
                    ..
                } if *asked_in == turn_id => Some((
                    Told::Approval(approval.id),
                    SidekickIntervention::Approval,
                    self.own_subagent(),
                )),
                _ => None,
            });
        let beneath = snapshot
            .subagent_interventions
            .iter()
            .filter(|asking| spawning_turn(snapshot, asking.via_session_id) == Some(turn_id))
            .flat_map(|asking| {
                let subagent = Some(asking.session_id);
                asking
                    .pending_questionnaires
                    .iter()
                    .map(move |id| {
                        (
                            Told::Questionnaire(*id),
                            SidekickIntervention::Questionnaire,
                            subagent,
                        )
                    })
                    .chain(asking.pending_approvals.iter().map(move |id| {
                        (
                            Told::Approval(*id),
                            SidekickIntervention::Approval,
                            subagent,
                        )
                    }))
            });
        for (told, intervention, subagent) in own.chain(beneath).collect::<Vec<_>>() {
            if work.told.contains(&told) || work.told.len() >= TOLD_INTERVENTIONS {
                continue;
            }
            work.told.push(told);
            reports.push((
                work.sidekick,
                SidekickReport::intervention_owed(self.subject(work, subagent), intervention),
            ));
        }
    }

    /// Whether anything the Turn `turn_id` spawned still works on what that
    /// Turn gave it: a Subagent whose latest row stands in it and has yet to
    /// settle, or whose Session the Remote says still works.
    fn branch_works_on(&self, turn_id: TurnId) -> bool {
        let snapshot = self.snapshot;
        snapshot.activities.iter().any(|activity| {
            let Activity::Subagent {
                session_id: subagent,
                turn_id: spawned_in,
                status,
                ..
            } = activity
            else {
                return false;
            };
            *spawned_in == turn_id
                && spawning_turn(snapshot, *subagent) == Some(turn_id)
                && (*status == ActivityStatus::Active || self.working.contains(subagent))
        })
    }
}

/// The Turn of `snapshot` that took `prompt`, delivered there: the Turn
/// begun for it, or else the Turn the user Message delivered from it stands
/// in — the latest such Message, should the same words have been sent again.
fn taking_turn(snapshot: &SessionSnapshot, prompt: &Prompt) -> Option<TurnId> {
    snapshot
        .turns
        .iter()
        .find(|turn| turn.prompt_id == Some(prompt.id))
        .map(|turn| turn.id)
        .or_else(|| {
            snapshot
                .messages
                .iter()
                .rev()
                .find(|message| {
                    message.role == MessageRole::User
                        && message.content == prompt.text
                        && message.author == prompt.author
                })
                .map(|message| message.turn_id)
        })
}
