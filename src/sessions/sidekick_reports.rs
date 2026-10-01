//! Sidekick Reports owed, and raised (CONTEXT.md: Sidekick Report).
//!
//! A Sidekick is owed a Report of a Session once it has a hand in that
//! Session's work: once it begins the Session, sends it a Prompt, or answers
//! its Questionnaire. Reading, listing, interrupting, settling or unsettling a
//! Session sets nothing going, and owes nothing. The obligation is the
//! Session's, kept here beside it in memory, so it is lost with everything
//! else held when Suru stops, and it goes with the Session when that Session
//! is deleted; a Sidekick whose own Session is deleted is owed nothing more.
//!
//! While it is owed, a Report is raised — held for the Sidekick's Agent, to
//! be delivered as a Subagent Report is (see [`SessionStoreState::hold_report`])
//! — whenever the Session, or a Subagent's Session beneath it, comes to owe a
//! Questionnaire or an Approval, once for each; and when the work the
//! Sidekick set going settles, which ends the obligation. That is the first
//! Turn of the Session to settle once nothing the Sidekick sent waits to be
//! delivered and no Turn holding what it sent is still working: the Turn its
//! Prompt began or steered, the Turn its Answer went on, or the Turn a Session
//! it began opened with. So a Sidekick hears once of each Turn it had a hand
//! in, and not of the Turns the user begins there after, nor of a Session's
//! work it only looked at; a Prompt the Sidekick queued behind the user's own
//! Turn has it hear of its own Turn rather than the user's. A Prompt it sent
//! that is withdrawn before any Turn takes it — the Session left with nothing
//! working — ends the obligation with nothing to tell. A Turn a restart
//! settles tells no one: what was owed before the stop was lost with it.
//!
//! Nothing here reaches a Transcript: a Report stands in none.

use crate::protocol::{
    Activity, ApprovalOutcome, Author, MessageRole, Outlook, PromptStatus, QuestionnaireOutcome,
    SessionChange, SessionId, SessionReference, SessionSnapshot, Turn, TurnId, TurnStatus,
};
use crate::provider::{SidekickIntervention, SidekickReport, SidekickTurnOutcome};
use crate::session_projection::agent_reading;

use super::{SessionStore, SessionStoreState, brokered::turn_failure, projection::active_turn_id};

impl SessionStore {
    /// Owes the Sidekick `author` names a Report of `session_id`, now that it
    /// has begun the Session, sent it a Prompt, or is answering its
    /// Questionnaire. Nothing is owed where the user acts for themselves.
    /// Answers whether the Sidekick is newly owed one, rather than owed one
    /// already or not at all.
    pub(crate) fn owe_sidekick_report(
        &self,
        session_id: SessionId,
        author: Option<&Author>,
    ) -> bool {
        let Some(Author::Sidekick {
            session_id: sidekick,
            ..
        }) = author
        else {
            return false;
        };
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get_mut(&session_id) else {
            return false;
        };
        if record.sidekicks_owed.contains(sidekick) {
            return false;
        }
        record.sidekicks_owed.push(*sidekick);
        true
    }

    /// Takes back the Report [`Self::owe_sidekick_report`] newly owed the
    /// Sidekick `author` names of `session_id`, for an act that did not take:
    /// an Answer the Questionnaire refused.
    pub(crate) fn forgive_sidekick_report(&self, session_id: SessionId, author: Option<&Author>) {
        let Some(Author::Sidekick {
            session_id: sidekick,
            ..
        }) = author
        else {
            return;
        };
        if let Some(record) = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get_mut(&session_id)
        {
            record.sidekicks_owed.retain(|owed| owed != sidekick);
        }
    }
}

impl SessionStoreState {
    /// Raises the Sidekick Reports a commit to `session_id` owes: one for each
    /// Questionnaire or Approval it newly asks, to every Sidekick owed a Report
    /// of it or of a Session above it, and one for a Turn it settled, to each
    /// Sidekick whose work that settles — which then is owed nothing more. A
    /// Turn in `repaired`, which a restart settled, raises nothing.
    pub(super) fn follow_sidekick_reports(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        self.raise_owed_interventions(session_id, changes);
        self.raise_settled_turn(session_id, changes, repaired);
    }

    /// Drops every Report owed to a Sidekick whose Session is among `deleted`:
    /// there is no Agent left to tell.
    pub(super) fn forget_sidekicks(&mut self, deleted: &[SessionId]) {
        for record in self.sessions.values_mut() {
            record
                .sidekicks_owed
                .retain(|sidekick| !deleted.contains(sidekick));
        }
    }

    /// Tells every Sidekick owed a Report of `session_id`, or of a Session
    /// above it, of each Intervention `changes` newly ask there — once, by the
    /// nearest Session it is owed one of, naming `session_id` as the
    /// Subagent's Session that owes it where that is not the Session itself.
    fn raise_owed_interventions(&mut self, session_id: SessionId, changes: &[SessionChange]) {
        let asked = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::ActivityAdded {
                    activity:
                        Activity::Questionnaire {
                            outcome: QuestionnaireOutcome::Pending,
                            ..
                        },
                } => Some(SidekickIntervention::Questionnaire),
                SessionChange::ActivityAdded {
                    activity:
                        Activity::Approval {
                            outcome: ApprovalOutcome::Pending,
                            ..
                        },
                } => Some(SidekickIntervention::Approval),
                _ => None,
            })
            .collect::<Vec<_>>();
        if asked.is_empty() {
            return;
        }
        let mut told = Vec::new();
        let mut reports = Vec::new();
        for (reported, record) in self.ancestors(session_id) {
            for sidekick in &record.sidekicks_owed {
                if told.contains(sidekick) {
                    continue;
                }
                told.push(*sidekick);
                for intervention in &asked {
                    reports.push((
                        *sidekick,
                        SidekickReport::intervention_owed(
                            SessionReference::new(Outlook::Local, reported),
                            record.snapshot.title.clone(),
                            *intervention,
                            (reported != session_id).then_some(session_id),
                        ),
                    ));
                }
            }
        }
        for (sidekick, report) in reports {
            self.hold_report(sidekick, report);
        }
    }

    /// Tells each Sidekick owed a Report of `session_id` whose work there has
    /// settled, and owes it nothing more: of the latest Turn `changes` settle,
    /// once nothing it sent waits to be delivered and no Turn holding what it
    /// sent works on. With no Turn settled and nothing working — what it sent
    /// withdrawn before a Turn took it — it is owed nothing more and told
    /// nothing.
    fn raise_settled_turn(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        let Some(record) = self.sessions.get(&session_id) else {
            return;
        };
        if record.sidekicks_owed.is_empty() {
            return;
        }
        let snapshot = &record.snapshot;
        let settled = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::TurnAdded { turn } if turn.status.is_terminal() => Some(turn.id),
                SessionChange::TurnStatusChanged {
                    turn_id, status, ..
                } if status.is_terminal() => Some(*turn_id),
                _ => None,
            })
            .rfind(|turn_id| !repaired.contains(turn_id))
            .and_then(|turn_id| snapshot.turns.iter().find(|turn| turn.id == turn_id));
        let working = active_turn_id(snapshot).ok().flatten();
        let mut done = Vec::new();
        let mut reports = Vec::new();
        for sidekick in &record.sidekicks_owed {
            if waits_on(snapshot, *sidekick, working) {
                continue;
            }
            match settled.and_then(|turn| settled_report(session_id, snapshot, turn)) {
                Some(report) => {
                    reports.push((*sidekick, report));
                    done.push(*sidekick);
                }
                None if working.is_none() => done.push(*sidekick),
                None => {}
            }
        }
        if done.is_empty() {
            return;
        }
        if let Some(record) = self.sessions.get_mut(&session_id) {
            record
                .sidekicks_owed
                .retain(|sidekick| !done.contains(sidekick));
        }
        for (sidekick, report) in reports {
            self.hold_report(sidekick, report);
        }
    }
}

/// Whether work `sidekick` set going in `snapshot` has yet to settle: a
/// Prompt it sent waits to be delivered, or the Turn `working` holds a
/// Message it sent.
fn waits_on(snapshot: &SessionSnapshot, sidekick: SessionId, working: Option<TurnId>) -> bool {
    let sent_by = |author: Option<&Author>| matches!(author, Some(Author::Sidekick { session_id, .. }) if *session_id == sidekick);
    snapshot
        .prompts
        .iter()
        .any(|prompt| prompt.status == PromptStatus::Pending && sent_by(prompt.author.as_ref()))
        || working.is_some_and(|turn_id| {
            snapshot.messages.iter().any(|message| {
                message.turn_id == turn_id
                    && message.role == MessageRole::User
                    && sent_by(message.author.as_ref())
            })
        })
}

/// The Report that `turn` of `session_id` settled, as `snapshot` holds it:
/// how it settled and after how long, what it failed with, and the final
/// Message its Agent wrote in it. `None` for a Turn still at work.
fn settled_report(
    session_id: SessionId,
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
        SessionReference::new(Outlook::Local, session_id),
        snapshot.title.clone(),
        outcome,
        turn.worked_ms(),
        turn_failure(snapshot, turn),
        agent_reading::final_message(snapshot, turn.id),
    ))
}
