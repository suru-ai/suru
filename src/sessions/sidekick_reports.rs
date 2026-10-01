//! Sidekick Reports owed, and raised (CONTEXT.md: Sidekick Report).
//!
//! A Sidekick is owed Reports of the work it sets going, piece by piece, and
//! of nothing else. Each piece is held against the Session it went to, as
//! what the Sidekick contributed there:
//!
//! - a Prompt it sent — the first of a Session it began among them — owed
//!   from the moment it is admitted, in the same step that admits it, until a
//!   Turn takes it; one withdrawn or lost before any Turn takes it is owed
//!   nothing more;
//! - the Turn that takes such a Prompt, or the Turn an Answer it gave went on
//!   in — owed once that Answer is delivered, in the same step that records
//!   it, so an Answer that never reached the Agent is owed nothing — which is
//!   reported when it settles;
//! - and the Subagents that Turn spawned, while any works on after the Turn
//!   itself settled: what they come to owe is reported until the whole branch
//!   the Turn set going has settled.
//!
//! While a Turn of the Sidekick's is working, or its branch works on, each
//! Questionnaire or Approval it — or a Subagent beneath it — comes to owe is
//! reported once. A Turn the user, or anyone else, begins in the same Session
//! is no work of the Sidekick's, so it tells the Sidekick nothing, whatever
//! else it set going there. Reading, listing, interrupting, settling or
//! unsettling a Session sets nothing going.
//!
//! All of it is held in memory beside the Session: lost with everything else
//! held when Suru stops, so a Turn a restart settles tells no one; gone with
//! the Session when it is deleted; and dropped when the Sidekick's own
//! Session is. It is apart from the stored record of the Sidekick's acts its
//! tree lists (see `sidekick_acts`), which outlives a stop and counts every
//! act. Like that record, which lists a Session acted on by the top-level
//! Session heading its tree, a Report names the top-level Session it is
//! about, and the Subagent's Session where what it tells of happened in one.
//!
//! Nothing here reaches a Transcript: a Report stands in none.

use crate::protocol::{
    Activity, ActivityStatus, ApprovalOutcome, MessageRole, Outlook, PromptId, PromptStatus,
    QuestionnaireOutcome, SessionChange, SessionId, SessionReference, SessionSnapshot, Turn,
    TurnId, TurnStatus,
};
use crate::provider::{
    SidekickIntervention, SidekickReport, SidekickReportSubject, SidekickTurnOutcome,
};
use crate::session_projection::agent_reading;

use super::{SessionRecord, SessionStoreState, brokered::turn_failure};

/// One piece of work a Sidekick set going in a Session, of which it is owed
/// Reports for as long as it lasts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SidekickWork {
    /// The Sidekick's own Session.
    sidekick: SessionId,
    stage: WorkStage,
}

/// How far a piece of a Sidekick's work has gone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkStage {
    /// A Prompt it sent, which no Turn has taken yet.
    Sent(PromptId),
    /// A Turn that took a Prompt it sent, or that an Answer it gave went on
    /// in, still working.
    Working(TurnId),
    /// Such a Turn, settled and reported, whose Subagents work on.
    Delegated(TurnId),
}

impl SidekickWork {
    /// The Prompt `prompt_id`, which the Sidekick of `sidekick` sent.
    pub(super) fn sent(sidekick: SessionId, prompt_id: PromptId) -> Self {
        Self {
            sidekick,
            stage: WorkStage::Sent(prompt_id),
        }
    }
}

impl SessionRecord {
    /// Holds `work` among what this Session's Sidekicks set going, unless the
    /// same Sidekick already holds that very piece: a Turn it both steered
    /// and answered is one Turn to be told of.
    fn hold_work(&mut self, work: SidekickWork) {
        if !self.sidekick_work.contains(&work) {
            self.sidekick_work.push(work);
        }
    }
}

impl SessionStoreState {
    /// Follows the Sidekicks' work in `session_id` through a commit of
    /// `changes` there: the Answers it delivered and the Prompts it moved, the
    /// Interventions it asked, and the Turns it settled — reporting each owed
    /// one — and lets go of the branches beneath that have settled. A Turn in
    /// `repaired`, which a restart settled, raises nothing.
    pub(super) fn follow_sidekick_reports(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        self.take_delivered_answers(session_id, changes);
        self.follow_sent_prompts(session_id, changes);
        self.raise_owed_interventions(session_id, changes);
        self.raise_settled_turns(session_id, changes, repaired);
        self.let_go_of_settled_branches(session_id);
    }

    /// Drops every piece of work of a Sidekick whose Session is among
    /// `deleted`: there is no Agent left to tell.
    pub(super) fn forget_sidekicks(&mut self, deleted: &[SessionId]) {
        for record in self.sessions.values_mut() {
            record
                .sidekick_work
                .retain(|work| !deleted.contains(&work.sidekick));
        }
    }

    /// Holds the Turn each Answer a Sidekick gave in `session_id` went on in,
    /// for each Answer `changes` record as delivered to the Agent.
    fn take_delivered_answers(&mut self, session_id: SessionId, changes: &[SessionChange]) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        for change in changes {
            let SessionChange::QuestionnaireSettled {
                activity_id,
                answer: Some(_),
                author: Some(author),
                ..
            } = change
            else {
                continue;
            };
            let (Some(sidekick), Some(turn_id)) = (
                author.sidekick_session(),
                record
                    .snapshot
                    .activities
                    .iter()
                    .find(|activity| activity.id() == *activity_id)
                    .map(Activity::turn_id),
            ) else {
                continue;
            };
            record.hold_work(SidekickWork {
                sidekick,
                stage: WorkStage::Working(turn_id),
            });
        }
    }

    /// Moves each Prompt a Sidekick sent `session_id` that `changes` deliver
    /// on to the Turn that took it, and lets go of each they withdraw — or that
    /// no longer waits for any other reason — with nothing to tell.
    fn follow_sent_prompts(&mut self, session_id: SessionId, changes: &[SessionChange]) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let mut moved = Vec::new();
        for work in &record.sidekick_work {
            let WorkStage::Sent(prompt_id) = work.stage else {
                continue;
            };
            let taken_by = taking_turn(changes, prompt_id);
            let waits = record
                .snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending);
            if taken_by.is_some() || !waits {
                moved.push((*work, taken_by));
            }
        }
        for (work, taken_by) in moved {
            record.sidekick_work.retain(|held| *held != work);
            if let Some(turn_id) = taken_by {
                record.hold_work(SidekickWork {
                    sidekick: work.sidekick,
                    stage: WorkStage::Working(turn_id),
                });
            }
        }
    }

    /// Tells each Sidekick whose work the Intervention concerns of each one
    /// `changes` newly ask in `session_id`, once: one asked in a Turn of the
    /// Sidekick's, or anywhere beneath a Subagent such a Turn spawned while
    /// that Turn works or its branch works on.
    fn raise_owed_interventions(&mut self, session_id: SessionId, changes: &[SessionChange]) {
        let asked = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::ActivityAdded {
                    activity:
                        Activity::Questionnaire {
                            turn_id,
                            outcome: QuestionnaireOutcome::Pending,
                            ..
                        },
                } => Some((*turn_id, SidekickIntervention::Questionnaire)),
                SessionChange::ActivityAdded {
                    activity:
                        Activity::Approval {
                            turn_id,
                            outcome: ApprovalOutcome::Pending,
                            ..
                        },
                } => Some((*turn_id, SidekickIntervention::Approval)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if asked.is_empty() {
            return;
        }
        let subject = self.subject_of(session_id);
        let mut reports = Vec::new();
        for (turn_id, intervention) in asked {
            let mut told = Vec::new();
            for sidekick in self.sidekicks_concerned(session_id, turn_id) {
                if !told.contains(&sidekick) {
                    told.push(sidekick);
                    reports.push((
                        sidekick,
                        SidekickReport::intervention_owed(subject.clone(), intervention),
                    ));
                }
            }
        }
        for (sidekick, report) in reports {
            self.hold_report(sidekick, report);
        }
    }

    /// The Sidekicks whose work something asked in Turn `turn_id` of
    /// `session_id` is part of: those whose Turn it is, and — up the line of
    /// Sessions above it — those whose Turn spawned the Subagent the line
    /// passes through, while that Turn works or its branch works on.
    fn sidekicks_concerned(&self, session_id: SessionId, turn_id: TurnId) -> Vec<SessionId> {
        let mut concerned = Vec::new();
        let mut beneath = None;
        for (holder, record) in self.ancestors(session_id) {
            let turn = match beneath {
                None => Some(turn_id),
                Some(subagent) => spawning_turn(&record.snapshot, subagent),
            };
            if let Some(turn) = turn {
                concerned.extend(record.sidekick_work.iter().filter_map(|work| {
                    let concerns = match work.stage {
                        WorkStage::Working(working) => working == turn,
                        WorkStage::Delegated(delegated) => {
                            delegated == turn && self.branch_works_on(holder, delegated)
                        }
                        WorkStage::Sent(_) => false,
                    };
                    concerns.then_some(work.sidekick)
                }));
            }
            beneath = Some(holder);
        }
        concerned
    }

    /// Tells each Sidekick whose Turn `changes` settle in `session_id` how it
    /// settled, once for each such Turn, and holds on to its branch where
    /// Subagents it spawned work on.
    fn raise_settled_turns(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        let settled = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::TurnAdded { turn } if turn.status.is_terminal() => Some(turn.id),
                SessionChange::TurnStatusChanged {
                    turn_id, status, ..
                } if status.is_terminal() => Some(*turn_id),
                _ => None,
            })
            .filter(|turn_id| !repaired.contains(turn_id))
            .collect::<Vec<_>>();
        let Some(record) = self.sessions.get(&session_id) else {
            return;
        };
        let reported = record
            .sidekick_work
            .iter()
            .filter_map(|work| match work.stage {
                WorkStage::Working(turn_id) if settled.contains(&turn_id) => Some((*work, turn_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if reported.is_empty() {
            return;
        }
        let subject = self.subject_of(session_id);
        let mut reports = Vec::new();
        let mut moved = Vec::new();
        for (work, turn_id) in reported {
            let report = record
                .snapshot
                .turns
                .iter()
                .find(|turn| turn.id == turn_id)
                .and_then(|turn| settled_report(subject.clone(), &record.snapshot, turn));
            if let Some(report) = report {
                reports.push((work.sidekick, report));
            }
            moved.push((
                work,
                self.branch_works_on(session_id, turn_id)
                    .then_some(WorkStage::Delegated(turn_id)),
            ));
        }
        let record = self
            .sessions
            .get_mut(&session_id)
            .expect("the Session was just read");
        for (work, next) in moved {
            record.sidekick_work.retain(|held| *held != work);
            if let Some(stage) = next {
                record.hold_work(SidekickWork {
                    sidekick: work.sidekick,
                    stage,
                });
            }
        }
        for (sidekick, report) in reports {
            self.hold_report(sidekick, report);
        }
    }

    /// Lets go of each branch a Sidekick's settled Turn set going, in
    /// `session_id` or a Session above it, that has settled whole.
    fn let_go_of_settled_branches(&mut self, session_id: SessionId) {
        let lineage = self
            .ancestors(session_id)
            .map(|(holder, _)| holder)
            .collect::<Vec<_>>();
        for holder in lineage {
            let settled = self.sessions[&holder]
                .sidekick_work
                .iter()
                .filter(|work| match work.stage {
                    WorkStage::Delegated(turn_id) => !self.branch_works_on(holder, turn_id),
                    _ => false,
                })
                .copied()
                .collect::<Vec<_>>();
            if let Some(record) = self.sessions.get_mut(&holder) {
                record.sidekick_work.retain(|work| !settled.contains(work));
            }
        }
    }

    /// Whether anything the Turn `turn_id` of `session_id` spawned still
    /// works on what that Turn gave it: a Subagent whose row there — its
    /// latest, so a Subagent another Turn has since resumed is that Turn's —
    /// has yet to settle, or whose Session still works, a Subagent of its own
    /// included.
    fn branch_works_on(&self, session_id: SessionId, turn_id: TurnId) -> bool {
        let Some(record) = self.sessions.get(&session_id) else {
            return false;
        };
        record.snapshot.activities.iter().any(|activity| {
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
                && spawning_turn(&record.snapshot, *subagent) == Some(turn_id)
                && (*status == ActivityStatus::Active
                    || self
                        .sessions
                        .get(subagent)
                        .is_some_and(|subagent| subagent.summary.session.working_since.is_some()))
        })
    }

    /// What a Report of something in `session_id` is about: the top-level
    /// Session heading its tree, by that Session's Title, and `session_id`
    /// itself as the Subagent's Session beneath it where it is one.
    fn subject_of(&self, session_id: SessionId) -> SidekickReportSubject {
        let top_level = self.top_level_of(session_id).unwrap_or(session_id);
        SidekickReportSubject {
            session: SessionReference::new(Outlook::Local, top_level),
            title: self
                .sessions
                .get(&top_level)
                .map(|record| record.snapshot.title.clone())
                .unwrap_or_default(),
            subagent: (top_level != session_id).then_some(session_id),
        }
    }
}

/// The Turn `changes` deliver the Prompt `prompt_id` into, if they deliver it:
/// a Turn it begins, or the one it steers. Every delivery follows the
/// Prompt's change of status with the Turn it begins, where it begins one,
/// and the Message it becomes, before any other Prompt's change.
fn taking_turn(changes: &[SessionChange], prompt_id: PromptId) -> Option<TurnId> {
    let delivered = changes.iter().position(|change| {
        matches!(
            change,
            SessionChange::PromptStatusChanged {
                prompt_id: changed,
                status: PromptStatus::Delivered | PromptStatus::Failed,
            } if *changed == prompt_id
        )
    })?;
    changes[delivered + 1..]
        .iter()
        .take_while(|change| !matches!(change, SessionChange::PromptStatusChanged { .. }))
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } if turn.prompt_id == Some(prompt_id) => Some(turn.id),
            SessionChange::MessageAdded { message } if message.role == MessageRole::User => {
                Some(message.turn_id)
            }
            _ => None,
        })
}

/// The Turn of `snapshot` whose row leads into `subagent`: the latest such
/// row, since a Subagent resumed stands in a row of each Turn that resumed it.
fn spawning_turn(snapshot: &SessionSnapshot, subagent: SessionId) -> Option<TurnId> {
    snapshot
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Subagent {
                session_id,
                turn_id,
                ..
            } if *session_id == subagent => Some(*turn_id),
            _ => None,
        })
}

/// The Report that `turn` settled in the Session `snapshot` holds, about
/// `subject`: how it settled and after how long, what it failed with, and the
/// final Message its Agent wrote in it. `None` for a Turn still at work.
fn settled_report(
    subject: SidekickReportSubject,
    snapshot: &SessionSnapshot,
    turn: &Turn,
) -> Option<SidekickReport> {
    let outcome = match turn.status {
        TurnStatus::Active => return None,
        TurnStatus::Completed => SidekickTurnOutcome::Completed,
        TurnStatus::Failed => SidekickTurnOutcome::Failed,
        TurnStatus::Interrupted => SidekickTurnOutcome::Interrupted,
    };
    Some(SidekickReport::turn_settled(
        subject,
        outcome,
        turn.worked_ms(),
        turn_failure(snapshot, turn),
        agent_reading::final_message(snapshot, turn.id),
    ))
}
