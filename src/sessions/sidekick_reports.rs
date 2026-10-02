//! Sidekick Reports owed, and raised (CONTEXT.md: Sidekick Report).
//!
//! A Sidekick is owed Reports of the work it sets going, piece by piece, and
//! of nothing else. Each piece is held against the Session it went to, as
//! what the Sidekick contributed there:
//!
//! - a Prompt it sent — the first of a Session it began among them — owed
//!   from the moment it is admitted, in the same step that admits it, until a
//!   Turn takes it: a Turn begun for it, or the working Turn its Provider took
//!   it into as a steer. One no Turn takes — withdrawn, refused by the
//!   Provider, or still waiting when the Turn it was to steer settled, which
//!   records it there though the Agent never took it — set nothing going and
//!   is owed nothing more;
//! - the Turn that takes such a Prompt, or the Turn an Answer it gave went on
//!   in — owed once that Answer is delivered, in the same step that records
//!   it, so an Answer that never reached the Agent is owed nothing — which is
//!   reported when it settles;
//! - and the Subagents that Turn set working, while any works on after the
//!   Turn itself settled: what they come to owe is reported until the whole
//!   branch the Turn set going has settled.
//!
//! A Subagent's work belongs to whichever Turn most recently set it working:
//! the one that spawned it, or a later one — of any Session above it — that
//! resumed it. So what a Subagent comes to owe is told to the Sidekick whose
//! Turn that is, followed up through each such delegation, never through a
//! delegation since superseded. Nothing is owed a Sidekick whose own Session
//! is no longer held.
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
    Activity, ActivityStatus, ApprovalOutcome, Outlook, PromptId, PromptStatus,
    QuestionnaireOutcome, SessionChange, SessionId, SessionReference, SessionSnapshot, Turn,
    TurnId, TurnStatus,
};
use crate::provider::{
    SidekickIntervention, SidekickReport, SidekickReportSubject, SidekickTurnOutcome,
};
use crate::session_projection::agent_reading;

use super::{
    SessionRecord, SessionStoreState,
    brokered::{delegating_session, turn_failure},
};

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
    /// Moves the Prompt `prompt_id` a Sidekick sent `session_id` on to the
    /// Turn `turn_id`, which has just taken it: a Turn begun for it, or the
    /// working Turn its Provider took it into as a steer. It is called in the
    /// step that delivers the Prompt, before that delivery is committed, so a
    /// Turn begun for it that settled at its start is told in that same
    /// commit. Nothing else moves a Prompt on.
    pub(super) fn take_sidekick_prompt(
        &mut self,
        session_id: SessionId,
        prompt_id: PromptId,
        turn_id: TurnId,
    ) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let taken = record
            .sidekick_work
            .iter()
            .filter(|work| work.stage == WorkStage::Sent(prompt_id))
            .copied()
            .collect::<Vec<_>>();
        for work in taken {
            record.sidekick_work.retain(|held| *held != work);
            record.hold_work(SidekickWork {
                sidekick: work.sidekick,
                stage: WorkStage::Working(turn_id),
            });
        }
    }

    /// Follows the Sidekicks' work in `session_id` through a commit of
    /// `changes` there: the Answers it delivered and the Prompts it left
    /// untaken, the Interventions it asked, and the Turns it settled —
    /// reporting each owed one — and lets go of the branches beneath that
    /// have settled. A Turn in `repaired`, which a restart settled, raises
    /// nothing.
    pub(super) fn follow_sidekick_reports(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        self.take_delivered_answers(session_id, changes);
        self.let_go_of_untaken_prompts(session_id);
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
    /// for each Answer `changes` record as delivered to the Agent — unless
    /// the Sidekick's own Session has gone meanwhile, leaving no one to tell.
    fn take_delivered_answers(&mut self, session_id: SessionId, changes: &[SessionChange]) {
        let held = |sidekick: &SessionId| self.sessions.contains_key(sidekick);
        let answered = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::QuestionnaireSettled {
                    activity_id,
                    answer: Some(_),
                    author: Some(author),
                    ..
                } => author
                    .sidekick_session()
                    .filter(held)
                    .map(|sidekick| (sidekick, *activity_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        for (sidekick, activity_id) in answered {
            let Some(turn_id) = record
                .snapshot
                .activities
                .iter()
                .find(|activity| activity.id() == activity_id)
                .map(Activity::turn_id)
            else {
                continue;
            };
            record.hold_work(SidekickWork {
                sidekick,
                stage: WorkStage::Working(turn_id),
            });
        }
    }

    /// Lets go of each Prompt a Sidekick sent `session_id` that no longer
    /// waits for a Turn, none having taken it (see
    /// [`Self::take_sidekick_prompt`]): withdrawn, failed, or recorded in a
    /// Turn that settled without taking it. It set nothing going, so there is
    /// nothing to tell.
    fn let_go_of_untaken_prompts(&mut self, session_id: SessionId) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let waiting = |prompt_id: PromptId| {
            record
                .snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending)
        };
        let untaken = record
            .sidekick_work
            .iter()
            .filter(|work| matches!(work.stage, WorkStage::Sent(prompt_id) if !waiting(prompt_id)))
            .copied()
            .collect::<Vec<_>>();
        record.sidekick_work.retain(|work| !untaken.contains(work));
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
    /// `session_id` is part of: those whose Turn it is, and — up through each
    /// delegation that set the Session asking working, as it stands now —
    /// those whose Turn set that Subagent working, while that Turn works or
    /// its branch works on.
    fn sidekicks_concerned(&self, session_id: SessionId, turn_id: TurnId) -> Vec<SessionId> {
        let mut concerned = Vec::new();
        let mut stretch = Some((session_id, turn_id));
        // No line of delegations is longer than the Sessions held, so a
        // walk that loops ends as a broken line does.
        let mut remaining = self.sessions.len();
        while let Some((holder, turn)) = stretch {
            let Some(record) = self.sessions.get(&holder) else {
                break;
            };
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
            remaining = match remaining.checked_sub(1) {
                Some(remaining) => remaining,
                None => break,
            };
            stretch = self.delegation_of(holder, turn);
        }
        concerned
    }

    /// The Session, and its Turn, that set `session_id` working on its Turn
    /// `turn_id`: the Session whose Delegation opened that Turn — or, for a
    /// Turn no Delegation opened, the latest before it that one did — else
    /// the Session it was spawned beneath; by that Session's latest row leading
    /// into it, which stands in the Turn that delegated. `None` for a
    /// top-level Session, which no one sets working.
    fn delegation_of(&self, session_id: SessionId, turn_id: TurnId) -> Option<(SessionId, TurnId)> {
        let snapshot = &self.sessions.get(&session_id)?.snapshot;
        let parent = snapshot.session.parent?;
        let through = snapshot
            .turns
            .iter()
            .position(|turn| turn.id == turn_id)
            .map_or(snapshot.turns.len(), |index| index + 1);
        let delegator = snapshot.turns[..through]
            .iter()
            .rev()
            .find_map(|turn| delegating_session(snapshot, turn.id))
            .unwrap_or(parent);
        [delegator, parent].into_iter().find_map(|holder| {
            let row = spawning_turn(&self.sessions.get(&holder)?.snapshot, session_id)?;
            Some((holder, row))
        })
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        protocol::{
            ActivityId, AdmitPromptRequest, Answer, Author, CreateSessionRequest,
            ExecutionDirectory, InitialPrompt, PromptDelivery, QuestionAnswer, Questionnaire,
            QuestionnaireId, ResolvedWorkspace,
        },
        questionnaire::Question,
        sessions::{
            DeliveredTurnStatus, ProviderTurnOutcome, SessionStore, StoreOutcome,
            TrailingCommandOutput,
        },
        storage::{RestoredSessions, StorageRepository, StorageWriter},
    };

    fn asking(text: &str) -> InitialPrompt {
        InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// A first Prompt asking `text` of a Session in `workspace`.
    fn beginning(workspace: &Path, text: &str) -> CreateSessionRequest {
        CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: workspace.to_owned(),
            },
            prompt: asking(text),
        }
    }

    fn sidekick(session_id: SessionId) -> Author {
        Author::Sidekick {
            session_id,
            title: "Plan the work".to_owned(),
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

    /// Delivers `session_id`'s Prompt `prompt_id` to begin a Turn, answering
    /// that Turn.
    fn deliver(store: &SessionStore, session_id: SessionId, prompt_id: PromptId) -> TurnId {
        store
            .deliver_prompt(session_id, prompt_id, None, DeliveredTurnStatus::Active)
            .unwrap()
            .expect("the Prompt begins a Turn")
            .turn_id
    }

    /// A Session the user began in `workspace` asking `text`, its first Turn
    /// begun, and that Turn.
    fn working(store: &SessionStore, workspace: &Path, text: &str) -> (SessionId, TurnId) {
        let StoreOutcome::Created(snapshot) = store.create(beginning(workspace, text)).unwrap()
        else {
            panic!("the Session is begun afresh");
        };
        let session_id = snapshot.session.id;
        (
            session_id,
            deliver(store, session_id, snapshot.prompts[0].id),
        )
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

    /// Has `session_id`'s Turn `turn_id` ask a Questionnaire, answering its
    /// Activity.
    fn asks(store: &SessionStore, session_id: SessionId, turn_id: TurnId) -> ActivityId {
        let id = ActivityId::new();
        store
            .publish(
                session_id,
                vec![SessionChange::ActivityAdded {
                    activity: Activity::Questionnaire {
                        id,
                        turn_id,
                        questionnaire: Questionnaire {
                            id: QuestionnaireId::new(),
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
                    },
                }],
            )
            .unwrap();
        id
    }

    /// The work Sidekicks set going in `session_id` that they are owed
    /// Reports of.
    fn work_in(store: &SessionStore, session_id: SessionId) -> Vec<SidekickWork> {
        store.state.lock().unwrap().sessions[&session_id]
            .sidekick_work
            .clone()
    }

    /// The Reports held for the Agent of `session_id`, as it would read them.
    fn held_for(store: &SessionStore, session_id: SessionId) -> Vec<String> {
        store
            .take_held_reports(session_id)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A Sidekick's Prompt is owed in the step that admits it, before any
    /// change it makes can be seen: so the Turn it begins, asking at once, is
    /// told, with nothing outside the store's own admission to register it.
    #[tokio::test]
    async fn a_sidekicks_prompt_is_owed_in_the_very_step_that_admits_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, sidekick_turn) = working(&store, &workspace, "Plan the work");
        let (target, first) = working(&store, &workspace, "Run the auth suite.");
        completes(&store, target, first);
        let (_, mut updates) = {
            let feed = store.subscribe(target).expect("the Session is held");
            (feed.snapshot, feed.updates)
        };

        let StoreOutcome::Created(admission) = store
            .admit(
                target,
                AdmitPromptRequest {
                    prompt: asking("Fix the flaky login test."),
                    delivery: PromptDelivery::Steer,
                },
                Vec::new(),
                Some(sidekick(sidekick_id)),
            )
            .unwrap()
        else {
            panic!("the Prompt is admitted afresh");
        };
        assert!(
            updates.try_recv().is_ok(),
            "the admission is published as the store admits it"
        );
        assert_eq!(
            work_in(&store, target),
            [SidekickWork::sent(sidekick_id, admission.prompt.id)],
            "and the Prompt is owed by then, in that same step"
        );

        let turn = deliver(&store, target, admission.prompt.id);
        asks(&store, target, turn);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(
            held[0].contains("asks a Questionnaire"),
            "the Questionnaire the Prompt's Turn asked at once is told: {held:?}"
        );
        completes(&store, sidekick_id, sidekick_turn);

        writer.shutdown().await.unwrap();
    }

    /// A Session a Sidekick begins is owed in the step that begins it.
    #[tokio::test]
    async fn a_session_a_sidekick_begins_is_owed_in_the_very_step_that_begins_it() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");

        let StoreOutcome::Created(begun) = store
            .create_in(
                beginning(&workspace, "Fix the flaky login test."),
                ResolvedWorkspace::directory(workspace.clone()),
                Vec::new(),
                None,
                Some(sidekick(sidekick_id)),
            )
            .unwrap()
        else {
            panic!("the Session is begun afresh");
        };
        let begun_id = begun.session.id;
        assert_eq!(
            work_in(&store, begun_id),
            [SidekickWork::sent(sidekick_id, begun.prompts[0].id)]
        );
        let turn = deliver(&store, begun_id, begun.prompts[0].id);
        asks(&store, begun_id, turn);
        assert_eq!(held_for(&store, sidekick_id).len(), 1);

        writer.shutdown().await.unwrap();
    }

    /// An Answer whose delivery completes after its Sidekick's Session was
    /// deleted owes nothing: no Agent is left to tell, and nothing of the
    /// deleted Sidekick lingers to be owed.
    #[tokio::test]
    async fn an_answer_delivered_after_its_sidekick_was_deleted_owes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, sidekick_turn) = working(&store, &workspace, "Plan the work");
        completes(&store, sidekick_id, sidekick_turn);
        let (target, turn) = working(&store, &workspace, "Run the auth suite.");
        let activity_id = asks(&store, target, turn);

        // The Sidekick's Answer is on its way when its Session is deleted.
        store
            .publish(
                target,
                vec![SessionChange::QuestionnaireAccepted { activity_id }],
            )
            .unwrap();
        store
            .delete(sidekick_id)
            .expect("the Sidekick's Session is deleted");
        store
            .publish(
                target,
                vec![SessionChange::QuestionnaireSettled {
                    activity_id,
                    outcome: QuestionnaireOutcome::Answered,
                    answer: Some(Answer {
                        questions: vec![QuestionAnswer::Freeform {
                            text: "staging".to_owned(),
                        }],
                    }),
                    author: Some(sidekick(sidekick_id)),
                }],
            )
            .unwrap();
        assert_eq!(
            work_in(&store, target),
            [],
            "the delivered Answer owes the deleted Sidekick nothing"
        );

        writer.shutdown().await.unwrap();
    }

    /// Nor does a Prompt a Sidekick sends after its Session was deleted.
    #[tokio::test]
    async fn a_prompt_admitted_for_a_deleted_sidekick_owes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, sidekick_turn) = working(&store, &workspace, "Plan the work");
        completes(&store, sidekick_id, sidekick_turn);
        let (target, turn) = working(&store, &workspace, "Run the auth suite.");
        store
            .delete(sidekick_id)
            .expect("the Sidekick's Session is deleted");

        store
            .admit(
                target,
                AdmitPromptRequest {
                    prompt: asking("Fix the flaky login test."),
                    delivery: PromptDelivery::Steer,
                },
                Vec::new(),
                Some(sidekick(sidekick_id)),
            )
            .unwrap();
        assert_eq!(work_in(&store, target), []);
        completes(&store, target, turn);

        writer.shutdown().await.unwrap();
    }
}
