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
//! The rule is written once, here, over what it needs to know of a Server's
//! Sessions and where it holds the work owed in them (see [`WorkTree`]), and
//! fed from two places: this Server's own Sessions, followed live through the
//! store's commits (see [`SessionStoreState::follow_sidekick_reports`]), and
//! a Remote's, replayed from a reading of it in the order the Remote stamped
//! what happened there (see `remote_reports`). Where whether a Session still
//! works is not known, no branch is taken to have settled.
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
    Activity, ActivityId, ActivityStatus, ApprovalOutcome, Outlook, PromptId, PromptStatus,
    QuestionnaireOutcome, SessionChange, SessionId, SessionReference, SessionSnapshot, Turn,
    TurnId, TurnStatus,
};
use crate::provider::{
    SidekickIntervention, SidekickReport, SidekickReportSubject, SidekickTurnOutcome,
};
use crate::session_projection::agent_reading;

use super::{
    SessionStoreState,
    brokered::{delegating_session, turn_failure},
};

/// One piece of work a Sidekick set going in a Session, of which it is owed
/// Reports for as long as it lasts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SidekickWork {
    /// The Sidekick's own Session.
    pub(super) sidekick: SessionId,
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

/// What the rule needs to know of the Sessions of one Server to follow the
/// work Sidekicks set going in them, and where it holds that work: this
/// Server's own Sessions as the store holds them, or a Remote's as a reading
/// of it gives them.
pub(super) trait WorkTree {
    /// What the Session `session_id` holds, as far as it is known here: its
    /// Turns and who opened each, its Prompts, its rows into Subagents.
    fn snapshot(&self, session_id: SessionId) -> Option<&SessionSnapshot>;
    /// Whether anything in the subtree of `session_id` works now: `None`
    /// where that is not known, which never counts as its having settled.
    fn works_now(&self, session_id: SessionId) -> Option<bool>;
    /// The work Sidekicks set going in `session_id`.
    fn works(&self, session_id: SessionId) -> &[SidekickWork];
    fn works_mut(&mut self, session_id: SessionId) -> Option<&mut Vec<SidekickWork>>;
    /// Whether the Sidekick's own Session `sidekick` is held still: nothing
    /// is owed one that is not, there being no Agent left to tell.
    fn sidekick_held(&self, sidekick: SessionId) -> bool;
    /// How many Sessions there are: no line of delegations is longer, so
    /// every walk through them stops within it.
    fn bound(&self) -> usize;
}

/// One thing a Sidekick is owed a Report of, as the rule raises it, for
/// whoever holds the Sessions to put in words.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Owed {
    /// The Turn `turn_id` of `session_id`, the Sidekick's work, settled.
    Settled {
        sidekick: SessionId,
        session_id: SessionId,
        turn_id: TurnId,
    },
    /// `session_id` asked `intervention`, as its Activity `activity_id`, in
    /// work the Sidekick set going.
    Asked {
        sidekick: SessionId,
        session_id: SessionId,
        activity_id: ActivityId,
        intervention: SidekickIntervention,
    },
}

impl Owed {
    /// The Sidekick it is owed.
    pub(super) fn sidekick(self) -> SessionId {
        match self {
            Self::Settled { sidekick, .. } | Self::Asked { sidekick, .. } => sidekick,
        }
    }
}

/// Holds `work` among what Sidekicks set going in a Session, `works`, unless
/// the same Sidekick already holds that very piece: a Turn it both steered
/// and answered is one Turn to be told of.
fn hold_work(works: &mut Vec<SidekickWork>, work: SidekickWork) {
    if !works.contains(&work) {
        works.push(work);
    }
}

/// Moves the Prompt `prompt_id` a Sidekick sent `session_id` on to the Turn
/// `turn_id`, which took it: a Turn begun for it, or the working Turn its
/// Agent took it into as a steer. Nothing else moves a Prompt on.
pub(super) fn take_prompt(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    prompt_id: PromptId,
    turn_id: TurnId,
) {
    let Some(works) = tree.works_mut(session_id) else {
        return;
    };
    let taken = works
        .iter()
        .filter(|work| work.stage == WorkStage::Sent(prompt_id))
        .copied()
        .collect::<Vec<_>>();
    for work in taken {
        works.retain(|held| *held != work);
        hold_work(
            works,
            SidekickWork {
                sidekick: work.sidekick,
                stage: WorkStage::Working(turn_id),
            },
        );
    }
}

/// Holds the Turn `turn_id` of `session_id` as the Sidekick of `sidekick`'s
/// work, an Answer it gave having been delivered to the Agent there — unless
/// its own Session has gone meanwhile, leaving no one to tell.
pub(super) fn answer_delivered(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    sidekick: SessionId,
    turn_id: TurnId,
) {
    if !tree.sidekick_held(sidekick) {
        return;
    }
    if let Some(works) = tree.works_mut(session_id) {
        hold_work(
            works,
            SidekickWork {
                sidekick,
                stage: WorkStage::Working(turn_id),
            },
        );
    }
}

/// Lets go of each Prompt a Sidekick sent `session_id` that no longer waits
/// for a Turn, none having taken it (see [`take_prompt`]): withdrawn,
/// failed, or recorded in a Turn that settled without taking it. It set
/// nothing going, so there is nothing to tell.
pub(super) fn let_go_of_untaken_prompts(tree: &mut impl WorkTree, session_id: SessionId) {
    let Some(snapshot) = tree.snapshot(session_id) else {
        return;
    };
    let untaken = tree
        .works(session_id)
        .iter()
        .filter(|work| match work.stage {
            WorkStage::Sent(prompt_id) => !snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending),
            _ => false,
        })
        .copied()
        .collect::<Vec<_>>();
    if let Some(works) = tree.works_mut(session_id) {
        works.retain(|work| !untaken.contains(work));
    }
}

/// What each Sidekick whose work it concerns is owed of `intervention`,
/// asked in Turn `turn_id` of `session_id` as its Activity `activity_id`:
/// one each, where it was asked in a Turn of the Sidekick's, or anywhere
/// beneath a Subagent such a Turn set working while that Turn works or its
/// branch works on.
pub(super) fn intervention_asked(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    activity_id: ActivityId,
    intervention: SidekickIntervention,
) -> Vec<Owed> {
    let mut owed = Vec::new();
    for sidekick in sidekicks_concerned(tree, session_id, turn_id) {
        let asked = Owed::Asked {
            sidekick,
            session_id,
            activity_id,
            intervention,
        };
        if !owed.contains(&asked) {
            owed.push(asked);
        }
    }
    owed
}

/// What each Sidekick is owed of the Turns `settled` of `session_id`, each
/// of which it set to work: one each as it settles. The branch each set
/// going is held on to where its Subagents work on — or where whether they
/// do is not known.
pub(super) fn turns_settled(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    settled: &[TurnId],
) -> Vec<Owed> {
    let reported = tree
        .works(session_id)
        .iter()
        .filter_map(|work| match work.stage {
            WorkStage::Working(turn_id) if settled.contains(&turn_id) => Some((*work, turn_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut owed = Vec::new();
    let mut moved = Vec::new();
    for (work, turn_id) in reported {
        owed.push(Owed::Settled {
            sidekick: work.sidekick,
            session_id,
            turn_id,
        });
        let next = (branch_works_on(tree, session_id, turn_id) != Some(false))
            .then_some(WorkStage::Delegated(turn_id));
        moved.push((work, next));
    }
    if let Some(works) = tree.works_mut(session_id) {
        for (work, next) in moved {
            works.retain(|held| *held != work);
            if let Some(stage) = next {
                hold_work(
                    works,
                    SidekickWork {
                        sidekick: work.sidekick,
                        stage,
                    },
                );
            }
        }
    }
    owed
}

/// Lets go of each branch a Sidekick's settled Turn set going, in
/// `session_id` or a Session above it, known to have settled whole.
pub(super) fn let_go_of_settled_branches(tree: &mut impl WorkTree, session_id: SessionId) {
    for holder in lineage(tree, session_id) {
        let settled = tree
            .works(holder)
            .iter()
            .filter(|work| match work.stage {
                WorkStage::Delegated(turn_id) => {
                    branch_works_on(tree, holder, turn_id) == Some(false)
                }
                _ => false,
            })
            .copied()
            .collect::<Vec<_>>();
        if let Some(works) = tree.works_mut(holder) {
            works.retain(|work| !settled.contains(work));
        }
    }
}

/// `session_id` and every Session above it, nearest first.
fn lineage(tree: &impl WorkTree, session_id: SessionId) -> Vec<SessionId> {
    let mut lineage = vec![session_id];
    let mut at = session_id;
    while let Some(parent) = tree
        .snapshot(at)
        .and_then(|snapshot| snapshot.session.parent)
    {
        if lineage.len() > tree.bound() || lineage.contains(&parent) {
            break;
        }
        lineage.push(parent);
        at = parent;
    }
    lineage
}

/// The Sidekicks whose work something asked in Turn `turn_id` of
/// `session_id` is part of: those whose Turn it is, and — up through each
/// delegation that set the Session asking working, as it stands now — those
/// whose Turn set that Subagent working, while that Turn works or its branch
/// works on.
fn sidekicks_concerned(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
) -> Vec<SessionId> {
    let mut concerned = Vec::new();
    let mut stretch = Some((session_id, turn_id));
    let mut remaining = tree.bound();
    while let Some((holder, turn)) = stretch {
        concerned.extend(tree.works(holder).iter().filter_map(|work| {
            let concerns = match work.stage {
                WorkStage::Working(working) => working == turn,
                WorkStage::Delegated(delegated) => {
                    delegated == turn && branch_works_on(tree, holder, delegated) != Some(false)
                }
                WorkStage::Sent(_) => false,
            };
            (concerns && tree.sidekick_held(work.sidekick)).then_some(work.sidekick)
        }));
        remaining = match remaining.checked_sub(1) {
            Some(remaining) => remaining,
            None => break,
        };
        stretch = delegation_of(tree, holder, turn);
    }
    concerned
}

/// The Session, and its Turn, that set `session_id` working on its Turn
/// `turn_id`: the Session whose Delegation opened that Turn — or, for a Turn
/// no Delegation opened, the latest before it that one did — else the
/// Session it was spawned beneath; by that Session's latest row leading into
/// it, which stands in the Turn that delegated. `None` for a top-level
/// Session, which no one sets working.
fn delegation_of(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
) -> Option<(SessionId, TurnId)> {
    let snapshot = tree.snapshot(session_id)?;
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
        let row = spawning_turn(tree.snapshot(holder)?, session_id)?;
        Some((holder, row))
    })
}

/// Whether anything the Turn `turn_id` of `session_id` spawned still works
/// on what that Turn gave it: a Subagent whose row there — its latest, so a
/// Subagent another Turn has since resumed is that Turn's — has yet to
/// settle, or whose Session still works, a Subagent of its own included.
/// `None` where it may, not being known.
fn branch_works_on(tree: &impl WorkTree, session_id: SessionId, turn_id: TurnId) -> Option<bool> {
    let Some(snapshot) = tree.snapshot(session_id) else {
        return Some(false);
    };
    let mut known = true;
    for activity in &snapshot.activities {
        let Activity::Subagent {
            session_id: subagent,
            turn_id: spawned_in,
            status,
            ..
        } = activity
        else {
            continue;
        };
        if *spawned_in != turn_id || spawning_turn(snapshot, *subagent) != Some(turn_id) {
            continue;
        }
        if *status == ActivityStatus::Active {
            return Some(true);
        }
        match tree.works_now(*subagent) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => known = false,
        }
    }
    known.then_some(false)
}

impl WorkTree for SessionStoreState {
    fn snapshot(&self, session_id: SessionId) -> Option<&SessionSnapshot> {
        self.sessions
            .get(&session_id)
            .map(|record| &record.snapshot)
    }

    fn works_now(&self, session_id: SessionId) -> Option<bool> {
        Some(
            self.sessions
                .get(&session_id)
                .is_some_and(|record| record.summary.session.working_since.is_some()),
        )
    }

    fn works(&self, session_id: SessionId) -> &[SidekickWork] {
        self.sessions
            .get(&session_id)
            .map_or(&[], |record| &record.sidekick_work)
    }

    fn works_mut(&mut self, session_id: SessionId) -> Option<&mut Vec<SidekickWork>> {
        self.sessions
            .get_mut(&session_id)
            .map(|record| &mut record.sidekick_work)
    }

    fn sidekick_held(&self, sidekick: SessionId) -> bool {
        self.sessions.contains_key(&sidekick)
    }

    fn bound(&self) -> usize {
        self.sessions.len()
    }
}

impl SessionStoreState {
    /// Follows the Sidekicks' work in `session_id` through a commit of
    /// `changes` there: the Prompts it had Turns take, the Answers it
    /// delivered and the Prompts it left untaken, the Interventions it asked,
    /// and the Turns it settled — reporting each owed one — and lets go of
    /// the branches beneath that have settled. A Turn in `repaired`, which a
    /// restart settled, raises nothing.
    pub(super) fn follow_sidekick_reports(
        &mut self,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        for change in changes {
            if let SessionChange::PromptTaken { prompt_id, taking } = change {
                take_prompt(self, session_id, *prompt_id, taking.turn_id);
            }
        }
        for (sidekick, activity_id) in delivered_answers(changes) {
            let turn_id = self.snapshot(session_id).and_then(|snapshot| {
                snapshot
                    .activities
                    .iter()
                    .find(|activity| activity.id() == activity_id)
                    .map(Activity::turn_id)
            });
            if let Some(turn_id) = turn_id {
                answer_delivered(self, session_id, sidekick, turn_id);
            }
        }
        let_go_of_untaken_prompts(self, session_id);
        let mut owed = Vec::new();
        for (turn_id, activity_id, intervention) in asked_interventions(changes) {
            owed.extend(intervention_asked(
                self,
                session_id,
                turn_id,
                activity_id,
                intervention,
            ));
        }
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
        owed.extend(turns_settled(self, session_id, &settled));
        let_go_of_settled_branches(self, session_id);
        for owed in owed {
            if let Some(report) = self.local_report(owed) {
                self.hold_report(owed.sidekick(), report);
            }
        }
    }

    /// Drops every piece of work of a Sidekick whose Session is among
    /// `deleted`, here and at Remotes: there is no Agent left to tell.
    pub(super) fn forget_sidekicks(&mut self, deleted: &[SessionId]) {
        for record in self.sessions.values_mut() {
            record
                .sidekick_work
                .retain(|work| !deleted.contains(&work.sidekick));
        }
        self.forget_remote_sidekicks(deleted);
    }

    /// The Report `owed` of this Server's own Sessions is told in.
    fn local_report(&self, owed: Owed) -> Option<SidekickReport> {
        match owed {
            Owed::Settled {
                session_id,
                turn_id,
                ..
            } => {
                let snapshot = self.snapshot(session_id)?;
                let turn = snapshot.turns.iter().find(|turn| turn.id == turn_id)?;
                settled_report(self.subject_of(session_id), snapshot, turn)
            }
            Owed::Asked {
                session_id,
                intervention,
                ..
            } => Some(SidekickReport::intervention_owed(
                self.subject_of(session_id),
                intervention,
            )),
        }
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

/// Each Answer a Sidekick of this Server gave that `changes` record as
/// delivered to the Agent, by the Sidekick and the Questionnaire's Activity.
fn delivered_answers(changes: &[SessionChange]) -> Vec<(SessionId, ActivityId)> {
    changes
        .iter()
        .filter_map(|change| match change {
            SessionChange::QuestionnaireSettled {
                activity_id,
                answer: Some(_),
                author: Some(author),
                ..
            } => author
                .sidekick_session()
                .map(|sidekick| (sidekick, *activity_id)),
            _ => None,
        })
        .collect()
}

/// Each Questionnaire or Approval `changes` newly ask, by the Turn it was
/// asked in and its Activity.
fn asked_interventions(
    changes: &[SessionChange],
) -> Vec<(TurnId, ActivityId, SidekickIntervention)> {
    changes
        .iter()
        .filter_map(|change| match change {
            SessionChange::ActivityAdded {
                activity:
                    Activity::Questionnaire {
                        id,
                        turn_id,
                        outcome: QuestionnaireOutcome::Pending,
                        ..
                    },
            } => Some((*turn_id, *id, SidekickIntervention::Questionnaire)),
            SessionChange::ActivityAdded {
                activity:
                    Activity::Approval {
                        id,
                        turn_id,
                        outcome: ApprovalOutcome::Pending,
                        ..
                    },
            } => Some((*turn_id, *id, SidekickIntervention::Approval)),
            _ => None,
        })
        .collect()
}

/// The Turn of `snapshot` whose row leads into `subagent`: the latest such
/// row, since a Subagent resumed stands in a row of each Turn that resumed it.
pub(super) fn spawning_turn(snapshot: &SessionSnapshot, subagent: SessionId) -> Option<TurnId> {
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
pub(super) fn settled_report(
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
                        asked_at: None,
                        settled_at: None,
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
                    settled_at: None,
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

    /// The store says when each Questionnaire is asked and settles, on the
    /// clock it says when each Turn began and settled by — what a Peer's
    /// Server orders a Remote's Interventions against its Sidekick's work by.
    #[tokio::test]
    async fn an_intervention_is_stamped_when_asked_and_a_questionnaire_when_it_settles() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (session_id, turn_id) = working(&store, &workspace, "Run the auth suite.");
        let activity_id = asks(&store, session_id, turn_id);
        let stamped = |store: &SessionStore| {
            let snapshot = store.snapshot(session_id).expect("the Session is held");
            let started_at = snapshot.turns[0].started_at;
            snapshot
                .activities
                .iter()
                .find_map(|activity| match activity {
                    Activity::Questionnaire {
                        id,
                        asked_at,
                        settled_at,
                        ..
                    } if *id == activity_id => Some((started_at, *asked_at, *settled_at)),
                    _ => None,
                })
                .expect("the Questionnaire stands")
        };
        let (started_at, asked_at, settled_at) = stamped(&store);
        assert!(asked_at > started_at, "{started_at:?} {asked_at:?}");
        assert_eq!(settled_at, None, "it waits");

        store
            .publish(
                session_id,
                vec![SessionChange::QuestionnaireSettled {
                    activity_id,
                    outcome: QuestionnaireOutcome::Declined,
                    answer: None,
                    author: None,
                    settled_at: None,
                }],
            )
            .unwrap();
        let (_, asked_again, settled_at) = stamped(&store);
        assert_eq!(asked_again, asked_at);
        assert!(settled_at > asked_at, "{asked_at:?} {settled_at:?}");

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
