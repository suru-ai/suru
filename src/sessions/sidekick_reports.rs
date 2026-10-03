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
//! - the Subagents that Turn set working, while any works on after the
//!   Turn itself settled: what they come to owe is reported until the whole
//!   branch the Turn set going has settled;
//! - and the Watches that work left running — each a Sidekick's where it
//!   started while a Turn of the Sidekick's worked in its Session, or a
//!   Subagent such a Turn had set working did — for as long as one is live:
//!   a Continuation begun in the Watch's Session meanwhile, or that the last
//!   one's settling wakes, is a Turn of the Sidekick's like the one a Prompt
//!   of its began, reported as it settles and carrying the work on through
//!   the Watches it leaves in its turn. Where the last of them in a Session
//!   ends and no such Continuation comes of it — it woke no one, or settled
//!   within a Turn someone else began — the Sidekick is told so once, unless
//!   its own interrupt stopped them.
//!
//! A Continuation begun in the Session of such a Turn while a Subagent it set
//! working works on — or the first begun once the last has settled, with no
//! Turn of anyone else's between — is the Sidekick's likewise. No ending is
//! told of those: where no Continuation comes, nothing more does.
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
    QuestionnaireOutcome, SessionChange, SessionId, SessionReference, SessionSnapshot,
    SessionTimestamp, Turn, TurnId, TurnStatus,
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
    /// Subagents its settled Turns set working in the Session are followed
    /// for the Continuations they wake: they work on — or have settled, the
    /// Continuation that wakes yet to begin — and a Continuation begun
    /// meanwhile is the Sidekick's.
    FollowingSubagents,
    /// Watches its work left running in the Session, of which one at least
    /// is live — or may be, where that is not known: a Continuation begun
    /// meanwhile is the Sidekick's.
    Watching,
    /// The last such Watch settled waking the Agent while no Turn was
    /// active: the Continuation it wakes into is the Sidekick's.
    Woken,
}

/// How a Watch ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatchEnd {
    /// It settled, waking its Agent.
    Woke,
    /// It settled waking no one: stopped, or settling as nothing to tell.
    Silent,
    /// It was lost with the Provider process that ran it, as was any wake
    /// still to come of one settled before.
    Lost,
}

impl SidekickWork {
    /// The Prompt `prompt_id`, which the Sidekick of `sidekick` sent.
    pub(super) fn sent(sidekick: SessionId, prompt_id: PromptId) -> Self {
        Self {
            sidekick,
            stage: WorkStage::Sent(prompt_id),
        }
    }

    /// The Turn `turn_id`, at work, as the work of the Sidekick of
    /// `sidekick`.
    pub(super) fn working(sidekick: SessionId, turn_id: TurnId) -> Self {
        Self {
            sidekick,
            stage: WorkStage::Working(turn_id),
        }
    }

    /// Whether this piece of work is a settled Turn's Subagents working
    /// on.
    pub(super) fn is_delegated(&self) -> bool {
        matches!(self.stage, WorkStage::Delegated(_))
    }

    /// The Turn at work this piece of work is, where it is one.
    pub(super) fn working_turn(&self) -> Option<TurnId> {
        match self.stage {
            WorkStage::Working(turn_id) => Some(turn_id),
            _ => None,
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
    /// Whether a Watch the work of the Sidekick of `sidekick` left running
    /// in `session_id` is live now: `None` where that is not known, which
    /// never counts as their having ended.
    fn watches_live(&self, session_id: SessionId, sidekick: SessionId) -> Option<bool>;
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
pub(crate) enum Owed {
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
    /// The Watches the Sidekick's work left running in `session_id` ended
    /// with no Continuation of the Sidekick's to come of them: the last
    /// settling within a Turn someone else began, where
    /// `within_another_turn`, and waking no one otherwise.
    WatchesEnded {
        sidekick: SessionId,
        session_id: SessionId,
        within_another_turn: bool,
    },
}

impl Owed {
    /// The Sidekick it is owed.
    pub(crate) fn sidekick(self) -> SessionId {
        match self {
            Self::Settled { sidekick, .. }
            | Self::Asked { sidekick, .. }
            | Self::WatchesEnded { sidekick, .. } => sidekick,
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
/// asked in Turn `turn_id` of `session_id` as its Activity `activity_id`, at
/// the moment `at` — now, where `None`: one each, where it was asked in a
/// Turn of the Sidekick's, or anywhere beneath a Subagent such a Turn had set
/// working at that moment, while that Turn works or its branch works on.
pub(super) fn intervention_asked(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    activity_id: ActivityId,
    intervention: SidekickIntervention,
    at: Option<SessionTimestamp>,
) -> Vec<Owed> {
    let mut owed = Vec::new();
    for sidekick in sidekicks_concerned(tree, session_id, turn_id, at) {
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

/// What each Sidekick is owed of the Turns `settled` of `session_id` at the
/// moment `at` — now, where `None` — each of which it set to work: one each
/// as it settles. The branch each set going is held on to where its
/// Subagents work on, and where Watches it left running are live — or where
/// whether they do, or are, is not known.
pub(super) fn turns_settled(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    settled: &[TurnId],
    at: Option<SessionTimestamp>,
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
        let delegated = (branch_works_on(tree, session_id, turn_id, at) != Some(false))
            .then_some(WorkStage::Delegated(turn_id));
        // Subagents all known to have settled by then wake nothing after.
        let delegates = (delegated.is_some()
            && !subagents_settled_by(tree, session_id, turn_id, at))
        .then_some(WorkStage::FollowingSubagents);
        let watching = (tree.watches_live(session_id, work.sidekick) != Some(false))
            .then_some(WorkStage::Watching);
        moved.push((work, [delegated, delegates, watching]));
    }
    if let Some(works) = tree.works_mut(session_id) {
        for (work, next) in moved {
            works.retain(|held| *held != work);
            for stage in next.into_iter().flatten() {
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

/// Takes up that a Watch started in `session_id` at the moment `at` — now,
/// where `None` — while its Turn `active` worked, answering the Sidekicks
/// whose work left it running, each of which follows the Session's Watches
/// from then on: those whose Turn that is, or whose Turn had set that
/// Session working, a Subagent's. A Watch heard to start with no Turn active
/// was started in the Turn before, its Provider telling of it late, and is
/// the Sidekick's whose Prompt began that Turn.
pub(super) fn watch_started(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    active: Option<TurnId>,
    at: Option<SessionTimestamp>,
) -> Vec<SessionId> {
    let mut owners = Vec::new();
    let found = match active {
        Some(turn_id) => sidekicks_concerned(tree, session_id, turn_id, at),
        None => tree
            .snapshot(session_id)
            .and_then(|snapshot| {
                let prompt_id = snapshot.turns.last()?.prompt_id?;
                let prompt = snapshot
                    .prompts
                    .iter()
                    .find(|prompt| prompt.id == prompt_id)?;
                prompt.author.as_ref()?.sidekick_session()
            })
            .filter(|sidekick| tree.sidekick_held(*sidekick))
            .into_iter()
            .collect(),
    };
    for sidekick in found {
        if !owners.contains(&sidekick) {
            owners.push(sidekick);
        }
    }
    for sidekick in &owners {
        follow_watches(tree, session_id, *sidekick);
    }
    owners
}

/// Has the Sidekick of `sidekick` follow the Watches its work left running
/// in `session_id`, one of them being live: a wake the last to settle had
/// yet to bring is followed with them.
pub(super) fn follow_watches(tree: &mut impl WorkTree, session_id: SessionId, sidekick: SessionId) {
    if let Some(works) = tree.works_mut(session_id) {
        works.retain(|work| {
            *work
                != SidekickWork {
                    sidekick,
                    stage: WorkStage::Woken,
                }
        });
        hold_work(
            works,
            SidekickWork {
                sidekick,
                stage: WorkStage::Watching,
            },
        );
    }
}

/// Whether the Turn `turn_id` of `session_id` is work of the Sidekick of
/// `sidekick`: its own Turn there, or one a Turn of its had set working, a
/// Subagent's.
fn is_work_of(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    sidekick: SessionId,
) -> bool {
    sidekicks_concerned(tree, session_id, turn_id, None).contains(&sidekick)
}

/// Takes up that the Turn `turn_id` began in `session_id` at the moment `at`
/// — now, where `None` — for the Subagents Sidekicks' settled Turns set
/// working there and the Watches their work left running there. A
/// Continuation — a Turn nothing asked for, which no Delegation opened — is
/// a Turn of each such Sidekick's, as the Turn that set them going was; one
/// begun only to hold a Subagent's row is no Turn at all here. Any other
/// Turn is its own beginner's, and takes what Subagents already settled, or
/// a Watch already ended, had yet to wake the Agent with: a Sidekick whose
/// Watches have all ended by then is owed the telling that they ended
/// within it, unless the Turn is its own, and of its Subagents nothing is
/// told. Either way, Subagents still working and Watches still live are
/// followed on, and Watches not known to be are followed no further than
/// this Turn: a Continuation carries them on in what it leaves running
/// itself.
pub(super) fn turn_began(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
) -> Vec<Owed> {
    let Some(snapshot) = tree.snapshot(session_id) else {
        return Vec::new();
    };
    let Some(turn) = snapshot.turns.iter().find(|turn| turn.id == turn_id) else {
        return Vec::new();
    };
    if holds_only_rows(snapshot, turn) {
        return Vec::new();
    }
    let continuation = is_continuation(snapshot, turn);
    let delegating = tree
        .works(session_id)
        .iter()
        .filter(|work| work.stage == WorkStage::FollowingSubagents)
        .copied()
        .collect::<Vec<_>>();
    for work in delegating {
        let live = subagents_live(tree, session_id, work.sidekick, at);
        let held = tree.sidekick_held(work.sidekick);
        let is_own = is_work_of(tree, session_id, turn_id, work.sidekick);
        let Some(works) = tree.works_mut(session_id) else {
            continue;
        };
        // A Continuation is the Sidekick's, and what its Subagents wake
        // after it is too while they work on, or may. Any other Turn takes
        // what Subagents already settled had to say, and a Turn of someone
        // else's ends the following where they are not known to work on.
        let followed_on = if continuation && held {
            hold_work(works, SidekickWork::working(work.sidekick, turn_id));
            live != Some(false)
        } else if is_own {
            live != Some(false)
        } else {
            live == Some(true)
        };
        if !followed_on {
            works.retain(|held| *held != work);
        }
    }
    let watched = tree
        .works(session_id)
        .iter()
        .filter_map(|work| match work.stage {
            WorkStage::Watching => Some((*work, tree.watches_live(session_id, work.sidekick))),
            WorkStage::Woken => Some((*work, Some(false))),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut owed = Vec::new();
    for (work, live) in watched {
        let own = SidekickWork {
            sidekick: work.sidekick,
            stage: WorkStage::Working(turn_id),
        };
        let held = tree.sidekick_held(work.sidekick);
        let is_own = is_work_of(tree, session_id, turn_id, work.sidekick);
        let Some(works) = tree.works_mut(session_id) else {
            continue;
        };
        if continuation && held {
            hold_work(works, own);
        } else if !is_own && live != Some(true) && held {
            owed.push(Owed::WatchesEnded {
                sidekick: work.sidekick,
                session_id,
                within_another_turn: true,
            });
        }
        // Watches not known to be live are followed on through the Turn
        // that took them up, where one did, and no further otherwise.
        if live != Some(true) {
            works.retain(|held| *held != work);
        }
    }
    owed
}

/// Takes up that a Watch of `session_id` ended as `end`, for each Sidekick
/// whose work left Watches running there of which none is live now. With a
/// Turn active, the wake lands in that Turn: its own beginner's, so the
/// Sidekick is owed the telling that its Watches ended within it, unless the
/// Turn is the Sidekick's. With none, a Watch that woke its Agent leaves the
/// Continuation to come the Sidekick's; one that woke no one leaves nothing
/// to come, which the Sidekick is owed the telling of. A Sidekick among
/// `stopped_by`, whose own interrupt was stopping that Watch, is owed no
/// telling of its ending waking no one.
pub(super) fn watch_ended(
    tree: &mut impl WorkTree,
    session_id: SessionId,
    end: WatchEnd,
    stopped_by: &[SessionId],
) -> Vec<Owed> {
    let Some(snapshot) = tree.snapshot(session_id) else {
        return Vec::new();
    };
    let active = snapshot
        .turns
        .iter()
        .find(|turn| turn.status == TurnStatus::Active)
        .map(|turn| turn.id);
    let ended = tree
        .works(session_id)
        .iter()
        .filter(|work| match work.stage {
            WorkStage::Watching => tree.watches_live(session_id, work.sidekick) == Some(false),
            WorkStage::Woken => end == WatchEnd::Lost,
            _ => false,
        })
        .copied()
        .collect::<Vec<_>>();
    // Lost with its Provider process is any Continuation that Subagents
    // already settled had yet to wake.
    if end == WatchEnd::Lost {
        let_go_of_settled_subagents(tree, session_id);
    }
    let mut owed = Vec::new();
    for work in ended {
        let is_own =
            active.is_some_and(|active| is_work_of(tree, session_id, active, work.sidekick));
        let held = tree.sidekick_held(work.sidekick);
        let Some(works) = tree.works_mut(session_id) else {
            continue;
        };
        works.retain(|held| *held != work);
        if active.is_none() && end == WatchEnd::Woke {
            hold_work(
                works,
                SidekickWork {
                    sidekick: work.sidekick,
                    stage: WorkStage::Woken,
                },
            );
            continue;
        }
        let stopped = end == WatchEnd::Silent && stopped_by.contains(&work.sidekick);
        if !is_own && held && !stopped {
            owed.push(Owed::WatchesEnded {
                sidekick: work.sidekick,
                session_id,
                within_another_turn: active.is_some(),
            });
        }
    }
    owed
}

/// Lets go of the Watches each Sidekick's work left running in `session_id`
/// that are known to have ended, answering what each Sidekick is owed the
/// telling of: for a tree read whole at one moment, where no Watch's ending
/// is heard of as it happens.
pub(super) fn let_go_of_ended_watches(
    tree: &mut impl WorkTree,
    session_id: SessionId,
) -> Vec<Owed> {
    let ended = tree
        .works(session_id)
        .iter()
        .filter(|work| {
            work.stage == WorkStage::Watching
                && tree.watches_live(session_id, work.sidekick) == Some(false)
        })
        .copied()
        .collect::<Vec<_>>();
    let mut owed = Vec::new();
    for work in ended {
        if let Some(works) = tree.works_mut(session_id) {
            works.retain(|held| *held != work);
        }
        if tree.sidekick_held(work.sidekick) {
            owed.push(Owed::WatchesEnded {
                sidekick: work.sidekick,
                session_id,
                within_another_turn: false,
            });
        }
    }
    owed
}

/// Whether a Subagent the settled Turns of the Sidekick of `sidekick` set
/// working in `session_id` worked on at the moment `at` — now, where `None`:
/// `None` where it may have, not being known. Of a moment past, each row
/// says first, by when it set its Subagent working and how long that
/// worked.
fn subagents_live(
    tree: &impl WorkTree,
    session_id: SessionId,
    sidekick: SessionId,
    at: Option<SessionTimestamp>,
) -> Option<bool> {
    let mut known = true;
    for work in tree.works(session_id) {
        let WorkStage::Delegated(turn_id) = work.stage else {
            continue;
        };
        if work.sidekick != sidekick {
            continue;
        }
        if subagents_settled_by(tree, session_id, turn_id, at) {
            continue;
        }
        if subagent_worked_at(tree, session_id, turn_id, at) {
            return Some(true);
        }
        match branch_works_on(tree, session_id, turn_id, at) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => known = false,
        }
    }
    known.then_some(false)
}

/// Whether every Subagent the Turn `turn_id` of `session_id` set working is
/// known to have settled by the moment `at`, each by how long its row says
/// it worked: never known of now, where `at` is `None`, which the Sessions'
/// own liveness says instead.
fn subagents_settled_by(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
) -> bool {
    let (Some(at), Some(snapshot)) = (at, tree.snapshot(session_id)) else {
        return false;
    };
    snapshot.activities.iter().all(|activity| match activity {
        Activity::Subagent {
            turn_id: spawned_in,
            status,
            duration_ms,
            delegated_at,
            ..
        } if *spawned_in == turn_id => {
            *status != ActivityStatus::Active
                && delegated_at
                    .zip(*duration_ms)
                    .is_some_and(|(began, worked)| began.0.saturating_add(worked) <= at.0)
        }
        _ => true,
    })
}

/// Whether a Subagent the Turn `turn_id` of `session_id` set working is
/// known to have been working at the moment `at`, by its row: set working
/// by then, and working still or for longer than until then. Never known of
/// now, where `at` is `None`.
fn subagent_worked_at(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
) -> bool {
    let (Some(at), Some(snapshot)) = (at, tree.snapshot(session_id)) else {
        return false;
    };
    snapshot.activities.iter().any(|activity| match activity {
        Activity::Subagent {
            turn_id: spawned_in,
            status,
            duration_ms,
            delegated_at: Some(began),
            ..
        } if *spawned_in == turn_id && *began <= at => match duration_ms {
            Some(worked) => began.0.saturating_add(*worked) > at.0,
            None => *status == ActivityStatus::Active,
        },
        _ => false,
    })
}

/// Whether `turn` of the Session `snapshot` holds is a Continuation begun
/// only to hold a Subagent's row, settled in the commit that began it:
/// nothing was written or asked in it, so no one worked in it (as
/// `subagents::stretches_of_work` tells them apart).
fn holds_only_rows(snapshot: &SessionSnapshot, turn: &Turn) -> bool {
    turn.is_continuation()
        && turn.settled_at.is_some()
        && turn.started_at == turn.settled_at
        && !snapshot
            .messages
            .iter()
            .any(|message| message.turn_id == turn.id)
        && snapshot.activities.iter().all(|activity| {
            activity.turn_id() != turn.id || matches!(activity, Activity::Subagent { .. })
        })
        && snapshot.activities.iter().any(|activity| {
            activity.turn_id() == turn.id && matches!(activity, Activity::Subagent { .. })
        })
}

/// Lets go of the Subagents each Sidekick's settled Turns set working in
/// `session_id` that are known to have settled whole with no Continuation
/// begun since, answering each Sidekick that followed them: for a tree read
/// whole at one moment, which says of no Continuation yet to begin.
pub(super) fn let_go_of_settled_subagents(
    tree: &mut impl WorkTree,
    session_id: SessionId,
) -> Vec<SessionId> {
    let settled = tree
        .works(session_id)
        .iter()
        .filter(|work| {
            work.stage == WorkStage::FollowingSubagents
                && subagents_live(tree, session_id, work.sidekick, None) == Some(false)
        })
        .copied()
        .collect::<Vec<_>>();
    if let Some(works) = tree.works_mut(session_id) {
        works.retain(|work| !settled.contains(work));
    }
    settled.into_iter().map(|work| work.sidekick).collect()
}

/// Lets go of each branch a Sidekick's settled Turn set going, in
/// `session_id` or a Session above it, known to have settled whole. Where a
/// Turn is at work in the Session holding it, that Turn takes what the
/// Subagents had to say, so no Continuation they wake is followed for.
pub(super) fn let_go_of_settled_branches(tree: &mut impl WorkTree, session_id: SessionId) {
    for holder in lineage(tree, session_id) {
        let settled = tree
            .works(holder)
            .iter()
            .filter(|work| match work.stage {
                WorkStage::Delegated(turn_id) => {
                    branch_works_on(tree, holder, turn_id, None) == Some(false)
                }
                _ => false,
            })
            .copied()
            .collect::<Vec<_>>();
        if let Some(works) = tree.works_mut(holder) {
            works.retain(|work| !settled.contains(work));
        }
        let taken = tree.snapshot(holder).is_some_and(|snapshot| {
            snapshot
                .turns
                .iter()
                .any(|turn| turn.status == TurnStatus::Active)
        });
        if !taken {
            continue;
        }
        let spoken = settled
            .iter()
            .map(|work| work.sidekick)
            .filter(|sidekick| subagents_live(tree, holder, *sidekick, None) == Some(false))
            .collect::<Vec<_>>();
        if let Some(works) = tree.works_mut(holder) {
            works.retain(|work| {
                !(work.stage == WorkStage::FollowingSubagents && spoken.contains(&work.sidekick))
            });
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
/// `session_id` at the moment `at` — now, where `None` — is part of: those
/// whose Turn it is, and — up through each delegation that had set the
/// Session asking working at that moment — those whose Turn set that
/// Subagent working, while that Turn works or its branch works on.
fn sidekicks_concerned(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
) -> Vec<SessionId> {
    let mut concerned = Vec::new();
    let mut stretch = Some((session_id, turn_id));
    let mut remaining = tree.bound();
    while let Some((holder, turn)) = stretch {
        concerned.extend(tree.works(holder).iter().filter_map(|work| {
            let concerns = match work.stage {
                WorkStage::Working(working) => working == turn,
                WorkStage::Delegated(delegated) => {
                    delegated == turn && branch_works_on(tree, holder, delegated, at) != Some(false)
                }
                WorkStage::Sent(_)
                | WorkStage::FollowingSubagents
                | WorkStage::Watching
                | WorkStage::Woken => false,
            };
            (concerns && tree.sidekick_held(work.sidekick)).then_some(work.sidekick)
        }));
        remaining = match remaining.checked_sub(1) {
            Some(remaining) => remaining,
            None => break,
        };
        stretch = delegation_of(tree, holder, turn, at);
    }
    concerned
}

/// The Session, and its Turn, that had set `session_id` working on its Turn
/// `turn_id` at the moment `at` — now, where `None`: the Session whose
/// Delegation opened that Turn — or, for a Turn no Delegation opened, the
/// latest before it that one did — else the Session it was spawned beneath;
/// by that Session's latest row leading into it by then, which stands in the
/// Turn that delegated. `None` for a top-level Session, which no one sets
/// working.
fn delegation_of(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
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
        let row = spawning_turn_at(tree.snapshot(holder)?, session_id, at)?;
        Some((holder, row))
    })
}

/// Whether anything the Turn `turn_id` of `session_id` spawned still works
/// on what that Turn gave it at the moment `at` — now, where `None`: a
/// Subagent whose row there — its latest by then, so a Subagent another Turn
/// had resumed is that Turn's — has yet to settle, or whose Session still
/// works, a Subagent of its own included. `None` where it may, not being
/// known.
fn branch_works_on(
    tree: &impl WorkTree,
    session_id: SessionId,
    turn_id: TurnId,
    at: Option<SessionTimestamp>,
) -> Option<bool> {
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
        if *spawned_in != turn_id || spawning_turn_at(snapshot, *subagent, at) != Some(turn_id) {
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

    fn watches_live(&self, session_id: SessionId, sidekick: SessionId) -> Option<bool> {
        Some(self.sessions.get(&session_id).is_some_and(|record| {
            record
                .watches
                .values()
                .any(|watch| watch.sidekicks.contains(&sidekick))
        }))
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
    /// delivered and the Prompts it left untaken, the Turns it began, the
    /// Interventions it asked, and the Turns it settled — reporting each owed one — and lets go of
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
        self.adopt_watches(session_id);
        let mut owed = Vec::new();
        for change in changes {
            if let SessionChange::TurnAdded { turn } = change {
                owed.extend(turn_began(self, session_id, turn.id, None));
            }
        }
        for (turn_id, activity_id, intervention) in asked_interventions(changes) {
            owed.extend(intervention_asked(
                self,
                session_id,
                turn_id,
                activity_id,
                intervention,
                None,
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
        owed.extend(turns_settled(self, session_id, &settled, None));
        let_go_of_settled_branches(self, session_id);
        for owed in owed {
            if let Some(report) = self.local_report(owed) {
                self.hold_report(owed.sidekick(), report);
            }
        }
    }

    /// Makes each Watch live in `session_id` that started while a Turn at
    /// work there worked the work of every Sidekick whose Turn that has
    /// since become — its Prompt taken into it as a steer, or its Answer
    /// delivered in it — as one started after would be.
    fn adopt_watches(&mut self, session_id: SessionId) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let mut adopted = Vec::new();
        for work in &record.sidekick_work {
            let Some(turn) = work
                .working_turn()
                .and_then(|turn_id| record.snapshot.turns.iter().find(|turn| turn.id == turn_id))
            else {
                continue;
            };
            for watch in record.watches.values_mut() {
                if started_within(turn, watch.started_at)
                    && !watch.sidekicks.contains(&work.sidekick)
                {
                    watch.sidekicks.push(work.sidekick);
                    adopted.push(work.sidekick);
                }
            }
        }
        for sidekick in adopted {
            follow_watches(self, session_id, sidekick);
        }
    }

    /// Follows the Sidekicks' work in `session_id` through a Watch of its
    /// ending as `end`, which no commit tells of: reporting to each Sidekick
    /// whose Watches there have all ended, with no Continuation of its own to
    /// come of them, that they did — but for those among `stopped_by`, whose
    /// own interrupt was stopping it.
    pub(super) fn follow_ended_watch(
        &mut self,
        session_id: SessionId,
        end: WatchEnd,
        stopped_by: &[SessionId],
    ) {
        for owed in watch_ended(self, session_id, end, stopped_by) {
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
                sidekick,
                session_id,
                turn_id,
            } => {
                let snapshot = self.snapshot(session_id)?;
                let turn = snapshot.turns.iter().find(|turn| turn.id == turn_id)?;
                settled_report(
                    self.subject_of(session_id),
                    snapshot,
                    turn,
                    self.watches_left_by(session_id, sidekick),
                    subagents_live(self, session_id, sidekick, None) == Some(true),
                )
            }
            Owed::WatchesEnded {
                session_id,
                within_another_turn,
                ..
            } => Some(SidekickReport::watches_ended(
                self.subject_of(session_id),
                within_another_turn,
            )),
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

/// The Turn of `snapshot` whose row leads into `subagent` at the moment `at`
/// — now, where `None`: the latest such row that had set it working by then,
/// since a Subagent resumed stands in a row of each Turn that resumed it. A
/// row stood before Suru recorded when is taken to have stood before
/// anything else it says.
fn spawning_turn_at(
    snapshot: &SessionSnapshot,
    subagent: SessionId,
    at: Option<SessionTimestamp>,
) -> Option<TurnId> {
    snapshot
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Subagent {
                session_id,
                turn_id,
                delegated_at,
                ..
            } if *session_id == subagent
                && at.is_none_or(|at| delegated_at.unwrap_or(SessionTimestamp(0)) <= at) =>
            {
                Some(*turn_id)
            }
            _ => None,
        })
}

/// Whether a Watch heard to start at `started_at` started while `turn`
/// worked: from when it began — whenever that was, for one stood before
/// Suru recorded it — until it settled.
pub(super) fn started_within(turn: &Turn, started_at: SessionTimestamp) -> bool {
    turn.started_at.is_none_or(|began| began <= started_at)
        && turn.settled_at.is_none_or(|settled| started_at <= settled)
}

/// Whether `turn` of the Session `snapshot` holds is a Continuation: a Turn
/// nothing asked for, which no Delegation opened.
fn is_continuation(snapshot: &SessionSnapshot, turn: &Turn) -> bool {
    turn.is_continuation() && delegating_session(snapshot, turn.id).is_none()
}

/// The Report that `turn` settled in the Session `snapshot` holds, about
/// `subject`: how it settled and after how long, what it failed with, the
/// final Message its Agent wrote in it, whether it was a Continuation, and
/// `watches`: what each Watch the work of the Sidekick told left running in
/// that Session, live still, is doing — and whether Subagents its work set
/// working there work on. `None` for a Turn still at work.
pub(crate) fn settled_report(
    subject: SidekickReportSubject,
    snapshot: &SessionSnapshot,
    turn: &Turn,
    watches: Vec<String>,
    subagents_work_on: bool,
) -> Option<SidekickReport> {
    let outcome = match turn.status {
        TurnStatus::Active => return None,
        TurnStatus::Completed => SidekickTurnOutcome::Completed,
        TurnStatus::Failed => SidekickTurnOutcome::Failed,
        TurnStatus::Interrupted => SidekickTurnOutcome::Interrupted,
    };
    Some(
        SidekickReport::turn_settled(
            subject,
            outcome,
            turn.worked_ms(),
            turn_failure(snapshot, turn),
            agent_reading::final_message(snapshot, turn.id),
        )
        .left_working(is_continuation(snapshot, turn), watches, subagents_work_on),
    )
}

/// The Report of `turn`, settled, of `subject`, a Remote's Session holding
/// more than this Server reads of a Remote at once — read from an outline of
/// it, `snapshot`, which holds nothing its Agent wrote — told without its
/// final Message, and with `watches` and `subagents_work_on` as
/// [`settled_report`] tells them. `None` for a Turn still working.
pub(crate) fn settled_report_past_budget(
    subject: SidekickReportSubject,
    snapshot: &SessionSnapshot,
    turn: &Turn,
    watches: Vec<String>,
    subagents_work_on: bool,
) -> Option<SidekickReport> {
    let outcome = match turn.status {
        TurnStatus::Active => return None,
        TurnStatus::Completed => SidekickTurnOutcome::Completed,
        TurnStatus::Failed => SidekickTurnOutcome::Failed,
        TurnStatus::Interrupted => SidekickTurnOutcome::Interrupted,
    };
    Some(
        SidekickReport::turn_settled_past_budget(subject, outcome, turn.worked_ms()).left_working(
            is_continuation(snapshot, turn),
            watches,
            subagents_work_on,
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        protocol::{
            ActivityId, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, Answer, Author,
            CreateSessionRequest, ExecutionDirectory, InitialPrompt, ModelId, PromptDelivery,
            ProviderId, QuestionAnswer, Questionnaire, QuestionnaireId, ResolvedWorkspace,
        },
        provider::ProviderWatchId,
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

    /// A Session the Sidekick of `sidekick_id` began in `workspace`, its
    /// first Turn begun, and that Turn.
    fn begun_by(
        store: &SessionStore,
        workspace: &Path,
        sidekick_id: SessionId,
    ) -> (SessionId, TurnId) {
        let StoreOutcome::Created(begun) = store
            .create_in(
                beginning(workspace, "Build the release."),
                ResolvedWorkspace::directory(workspace.to_owned()),
                Vec::new(),
                None,
                Some(sidekick(sidekick_id)),
            )
            .unwrap()
        else {
            panic!("the Session is begun afresh");
        };
        let session_id = begun.session.id;
        (session_id, deliver(store, session_id, begun.prompts[0].id))
    }

    /// Has `session_id`'s Agent leave the Watch `watch` running.
    fn watches(store: &SessionStore, session_id: SessionId, watch: &str) -> ProviderWatchId {
        let watch_id = ProviderWatchId::new(watch.to_owned());
        store
            .start_watch(session_id, watch_id.clone(), watch.to_owned())
            .unwrap();
        watch_id
    }

    /// A Turn that settles leaving a Watch running is reported as it settles,
    /// saying the Session is Monitoring it; and the Continuation the Watch's
    /// settling wakes is the Sidekick's, reported as it settles, after which
    /// nothing more is owed.
    #[tokio::test]
    async fn a_continuation_a_watch_the_sidekicks_turn_left_wakes_is_reported_as_it_settles() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, target, "cargo build --release");

        completes(&store, target, turn);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(
            held[0].contains("has settled its Turn")
                && held[0].contains("It left Watches running")
                && held[0].contains("\"cargo build --release\""),
            "the Turn's settling says what it left running: {held:?}"
        );

        assert!(store.settle_watch(target, &build, true).is_some());
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());
        let continuation = store.begin_continuation(target, agent()).unwrap();
        completes(&store, target, continuation);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(
            held[0].contains("has settled a Continuation of the work you set going there")
                && !held[0].contains("left Watches running"),
            "the Continuation is told as one, with nothing left running: {held:?}"
        );
        assert_eq!(work_in(&store, target), [], "and nothing more is owed");

        writer.shutdown().await.unwrap();
    }

    /// A Continuation begun while a Watch of the Sidekick's Turn is live — a
    /// monitor reporting short of settling — is the Sidekick's too, and its
    /// Report says the Watch runs on; the next is as well.
    #[tokio::test]
    async fn a_continuation_begun_while_the_watch_runs_on_is_the_sidekicks_and_so_is_the_next() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        watches(&store, target, "tail -f deploy.log");
        completes(&store, target, turn);
        held_for(&store, sidekick_id);

        for _ in 0..2 {
            let continuation = store.begin_continuation(target, agent()).unwrap();
            completes(&store, target, continuation);
            let held = held_for(&store, sidekick_id);
            assert_eq!(held.len(), 1, "{held:?}");
            assert!(
                held[0].contains("has settled a Continuation")
                    && held[0].contains("It left Watches running")
                    && held[0].contains("\"tail -f deploy.log\""),
                "{held:?}"
            );
        }

        writer.shutdown().await.unwrap();
    }

    /// A Watch already running when the Sidekick's Turn began is none of the
    /// Sidekick's: its Turn's Report says nothing of it, and a Continuation
    /// it wakes tells the Sidekick nothing.
    #[tokio::test]
    async fn a_watch_running_before_the_sidekicks_turn_began_is_not_the_sidekicks() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, first) = working(&store, &workspace, "Start the dev server.");
        let server = watches(&store, target, "npm run dev");
        completes(&store, target, first);

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
        let turn = deliver(&store, target, admission.prompt.id);
        completes(&store, target, turn);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(!held[0].contains("left Watches running"), "{held:?}");
        assert_eq!(work_in(&store, target), []);

        store.settle_watch(target, &server, true);
        let continuation = store.begin_continuation(target, agent()).unwrap();
        completes(&store, target, continuation);
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());

        writer.shutdown().await.unwrap();
    }

    /// Watches that end waking no one — stopped, or lost with their Provider
    /// process, as is the wake one already settled had yet to bring — are
    /// told once, and nothing more is owed.
    #[tokio::test]
    async fn watches_that_end_waking_no_one_are_told_once() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let ended = |held: Vec<String>| {
            assert_eq!(held.len(), 1, "{held:?}");
            assert!(
                held[0].contains("ended without waking its Agent"),
                "{held:?}"
            );
        };

        let (stopped, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, stopped, "cargo build");
        let tests = watches(&store, stopped, "cargo test");
        completes(&store, stopped, turn);
        held_for(&store, sidekick_id);
        store.settle_watch(stopped, &build, false);
        assert_eq!(
            held_for(&store, sidekick_id),
            Vec::<String>::new(),
            "one of them runs on"
        );
        store.settle_watch(stopped, &tests, false);
        ended(held_for(&store, sidekick_id));
        assert_eq!(work_in(&store, stopped), []);

        let (lost, turn) = begun_by(&store, &workspace, sidekick_id);
        watches(&store, lost, "cargo build");
        completes(&store, lost, turn);
        held_for(&store, sidekick_id);
        store.lose_watches(lost);
        ended(held_for(&store, sidekick_id));

        let (woken, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, woken, "cargo build");
        completes(&store, woken, turn);
        held_for(&store, sidekick_id);
        store.settle_watch(woken, &build, true);
        store.lose_watches(woken);
        ended(held_for(&store, sidekick_id));
        assert_eq!(work_in(&store, woken), []);

        writer.shutdown().await.unwrap();
    }

    /// A Watch settling within a Turn someone else began is told once as
    /// that, and that Turn stays theirs: its settling tells the Sidekick
    /// nothing.
    #[tokio::test]
    async fn a_watch_settling_within_a_turn_someone_else_began_is_told_and_the_turn_stays_theirs() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let users_turn = |store: &SessionStore, session_id| {
            let StoreOutcome::Created(admission) = store
                .admit(
                    session_id,
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
            deliver(store, session_id, admission.prompt.id)
        };
        let within = |held: Vec<String>| {
            assert_eq!(held.len(), 1, "{held:?}");
            assert!(
                held[0].contains("settled within a Turn someone else began"),
                "{held:?}"
            );
        };

        // Settling while the user's Turn works.
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, target, "cargo build");
        completes(&store, target, turn);
        held_for(&store, sidekick_id);
        let theirs = users_turn(&store, target);
        assert_eq!(
            held_for(&store, sidekick_id),
            Vec::<String>::new(),
            "the Watch runs on beneath their Turn"
        );
        store.settle_watch(target, &build, true);
        within(held_for(&store, sidekick_id));
        completes(&store, target, theirs);
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());
        assert_eq!(work_in(&store, target), []);

        // Settled, its wake landing in the Turn the user began first.
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, target, "cargo build");
        completes(&store, target, turn);
        held_for(&store, sidekick_id);
        store.settle_watch(target, &build, true);
        let theirs = users_turn(&store, target);
        within(held_for(&store, sidekick_id));
        completes(&store, target, theirs);
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());

        writer.shutdown().await.unwrap();
    }

    /// A Sidekick that itself stops the Watches its work left running is
    /// told nothing of their ending; one whose stop stopped none is owed it
    /// still, and so is the Continuation of one that settled of itself first.
    #[tokio::test]
    async fn a_sidekick_stopping_its_own_watches_is_told_nothing_of_their_ending() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let left_watching = || {
            let (target, turn) = begun_by(&store, &workspace, sidekick_id);
            let build = watches(&store, target, "cargo build");
            completes(&store, target, turn);
            held_for(&store, sidekick_id);
            (target, build)
        };

        let (target, build) = left_watching();
        store.sidekick_stops_watches(sidekick_id, target);
        store.settle_watch(target, &build, false);
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());
        assert_eq!(work_in(&store, target), []);

        let (target, build) = left_watching();
        store.sidekick_stops_watches(sidekick_id, target);
        store.sidekick_stopped_no_watches(sidekick_id, target);
        store.settle_watch(target, &build, false);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "the stop stopped nothing: {held:?}");
        assert!(
            held[0].contains("ended without waking its Agent"),
            "{held:?}"
        );

        let (target, build) = left_watching();
        store.sidekick_stops_watches(sidekick_id, target);
        store.settle_watch(target, &build, true);
        let continuation = store.begin_continuation(target, agent()).unwrap();
        completes(&store, target, continuation);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(held[0].contains("has settled a Continuation"), "{held:?}");

        writer.shutdown().await.unwrap();
    }

    /// Watches left by a Turn and by the Continuation it woke into end as
    /// one: their ending is told once, when the last of them ends.
    #[tokio::test]
    async fn watches_left_across_a_turn_and_its_continuation_end_as_one() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        let build = watches(&store, target, "cargo build");
        completes(&store, target, turn);
        let continuation = store.begin_continuation(target, agent()).unwrap();
        let tests = watches(&store, target, "cargo test");
        completes(&store, target, continuation);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(
            held[1].contains("\"cargo build\"; \"cargo test\""),
            "the Continuation's Report names both: {held:?}"
        );

        store.settle_watch(target, &build, false);
        assert_eq!(held_for(&store, sidekick_id), Vec::<String>::new());
        store.settle_watch(target, &tests, false);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(
            held[0].contains("ended without waking its Agent"),
            "{held:?}"
        );
        assert_eq!(work_in(&store, target), []);

        writer.shutdown().await.unwrap();
    }

    /// A Watch started in a Turn before the Sidekick's steer was taken into
    /// it is the Sidekick's as the Turn is, it having started in that Turn.
    #[tokio::test]
    async fn a_watch_started_in_a_turn_before_it_took_the_sidekicks_steer_is_the_sidekicks() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, turn) = working(&store, &workspace, "Run the auth suite.");
        let build = watches(&store, target, "cargo build");
        let StoreOutcome::Created(admission) = store
            .admit(
                target,
                AdmitPromptRequest {
                    prompt: asking("And fix the flaky login test."),
                    delivery: PromptDelivery::Steer,
                },
                Vec::new(),
                Some(sidekick(sidekick_id)),
            )
            .unwrap()
        else {
            panic!("the Prompt is admitted afresh");
        };
        store
            .deliver_steer(target, turn, admission.prompt.id)
            .unwrap()
            .expect("the working Turn takes the steer");

        completes(&store, target, turn);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(held[0].contains("\"cargo build\""), "{held:?}");
        store.settle_watch(target, &build, true);
        let continuation = store.begin_continuation(target, agent()).unwrap();
        completes(&store, target, continuation);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(held[0].contains("has settled a Continuation"), "{held:?}");

        writer.shutdown().await.unwrap();
    }

    /// A Watch its Provider tells of only once its Turn has settled is the
    /// Sidekick's whose Prompt began that Turn.
    #[tokio::test]
    async fn a_watch_heard_of_after_its_turn_settled_is_that_turns_sidekicks() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let (sidekick_id, _) = working(&store, &workspace, "Plan the work");
        let (target, turn) = begun_by(&store, &workspace, sidekick_id);
        completes(&store, target, turn);
        held_for(&store, sidekick_id);

        let build = watches(&store, target, "cargo build");
        store.settle_watch(target, &build, true);
        let continuation = store.begin_continuation(target, agent()).unwrap();
        completes(&store, target, continuation);
        let held = held_for(&store, sidekick_id);
        assert_eq!(held.len(), 1, "{held:?}");
        assert!(held[0].contains("has settled a Continuation"), "{held:?}");

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
