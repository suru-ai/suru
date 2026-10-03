//! How the presentation tick promotes an Active Command into its live tail,
//! and how little it plans the Transcript to do so: once a promotion is
//! decided — made, or overridden by the reader — no tick plans for it again,
//! and one owed to a Command out of sight waits for the Transcript to move.

use std::{
    cell::Ref,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::{Application, ApplicationEvent};
use crate::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        Activity, ActivityId, ActivityStatus, CommandAutoExpand, EffectiveSettings, GroupPosture,
        ModelAvailability, Outlook, Session, SessionChange, SessionId, SessionRevision,
        SessionSnapshot, SessionStatus, SessionUpdate, SettingsSnapshot, TranscriptItem,
        TranscriptSettings, Turn, TurnId, TurnStatus, Workspace,
    },
    tui::transcript::{FoldStep, TranscriptFolds},
};

/// A client whose presentation clock reads `elapsed_ms` past a fixed origin,
/// under Settings that promote after `after_millis` and group as `groups`
/// says, with `snapshot` open in its main view.
fn application_with(
    elapsed_ms: &Arc<AtomicU64>,
    after_millis: u64,
    groups: GroupPosture,
    snapshot: SessionSnapshot,
) -> Application {
    let workspace = snapshot.session.execution_directory.path.clone();
    let observed = Arc::clone(elapsed_ms);
    let origin = Instant::now();
    let mut application =
        Application::new(workspace, Default::default()).with_presentation_clock(move || {
            origin + Duration::from_millis(observed.load(Ordering::Relaxed))
        });
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    transcript: TranscriptSettings {
                        command_auto_expand: CommandAutoExpand::AfterMillis(after_millis),
                        groups,
                        ..TranscriptSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the effective Settings");
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open the Session");
    application
}

/// A Working Session whose one Turn ran `commands`, each `(name, status)`.
fn session_running(commands: &[(&str, ActivityStatus)]) -> (SessionSnapshot, Vec<ActivityId>) {
    let workspace = std::env::temp_dir();
    let turn_id = TurnId::new();
    let activities = commands
        .iter()
        .map(|(name, status)| command(turn_id, name, *status))
        .collect::<Vec<_>>();
    let ids = activities.iter().map(Activity::id).collect::<Vec<_>>();
    let snapshot = SessionSnapshot {
        title: String::new(),
        icon: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: SessionId::new(),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: workspace.clone(),
            },
            workspace: Workspace::directory(workspace),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Active,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: vec![Turn {
            id: turn_id,
            prompt_id: None,
            compaction_requested: false,
            agent: None,
            status: TurnStatus::Active,
            started_at: None,
            settled_at: None,
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }],
        messages: Vec::new(),
        transcript: ids
            .iter()
            .map(|activity_id| TranscriptItem::Activity {
                activity_id: *activity_id,
            })
            .collect(),
        activities,
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: SessionRevision(0),
        watches: Vec::new(),
        waiting_on_subagents: None,
        subagent_usage: None,
        total_cost: None,
        own_cost: None,
        attachments: Vec::new(),
    };
    (snapshot, ids)
}

/// The same Session at the same revision with `activities` as its Turn's
/// work: a snapshot replacing it that nothing about its revision tells apart.
fn replaced(snapshot: &SessionSnapshot, activities: Vec<Activity>) -> SessionSnapshot {
    let mut replacement = snapshot.clone();
    replacement.transcript = activities
        .iter()
        .map(|activity| TranscriptItem::Activity {
            activity_id: activity.id(),
        })
        .collect();
    replacement.activities = activities;
    replacement
}

fn command(turn_id: TurnId, name: &str, status: ActivityStatus) -> Activity {
    Activity::Command {
        id: ActivityId::new(),
        turn_id,
        status,
        command: name.to_owned(),
        cwd: None,
        output: (1..=12)
            .map(|line| format!("{name} line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        output_truncated: false,
        exit_status: match status {
            ActivityStatus::Active | ActivityStatus::Interrupted => None,
            ActivityStatus::Completed | ActivityStatus::Failed => Some(0),
        },
    }
}

/// Lands one Session update carrying `change` at the next revision.
fn update(application: &mut Application, change: SessionChange) {
    let snapshot = application
        .state
        .session
        .as_ref()
        .expect("a Session is open")
        .snapshot();
    let (session_id, revision) = (snapshot.session.id, snapshot.revision);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![change],
            },
        )))
        .expect("apply the Session update");
}

fn tick_at(application: &mut Application, elapsed_ms: &AtomicU64, at: u64) {
    elapsed_ms.store(at, Ordering::Relaxed);
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("advance the presentation tick");
}

/// Opens the Group `leader` leads in the open Session's view, whether or not
/// that Session holds it yet.
fn open_group(application: &Application, leader: ActivityId) {
    let reference = application
        .state
        .session_reference
        .as_ref()
        .expect("a Session is open");
    application
        .state
        .session_interaction(reference)
        .expect("the open Session has view state")
        .groups
        .borrow_mut()
        .expand(leader);
}

fn folds(application: &Application) -> Ref<'_, TranscriptFolds> {
    let reference = application
        .state
        .session_reference
        .as_ref()
        .expect("a Session is open");
    application
        .state
        .session_interaction(reference)
        .expect("the open Session has view state")
        .folds
        .borrow()
}

/// The step a running Command presents at: Folded until something — a
/// promotion or the reader — sets it.
fn step(application: &Application, activity_id: ActivityId) -> FoldStep {
    folds(application).resolve(activity_id, FoldStep::Folded)
}

fn plans(application: &Application) -> usize {
    application.state.live_output_visibility.plans()
}

#[test]
fn a_promoted_command_never_plans_the_transcript_again() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[("cargo build", ActivityStatus::Active)]);
    let mut application = application_with(&elapsed_ms, 500, GroupPosture::Off, snapshot);

    tick_at(&mut application, &elapsed_ms, 499);
    assert_eq!(
        (step(&application, ids[0]), plans(&application)),
        (FoldStep::Folded, 0),
        "nothing is planned while no Command has aged"
    );

    tick_at(&mut application, &elapsed_ms, 500);
    assert_eq!(
        (step(&application, ids[0]), plans(&application)),
        (FoldStep::Peek, 1),
        "the Command that aged is promoted from one plan"
    );

    for at in (532..2_000).step_by(32) {
        update(
            &mut application,
            SessionChange::CommandOutputAppended {
                activity_id: ids[0],
                content: format!("\nstreamed at {at}"),
            },
        );
        tick_at(&mut application, &elapsed_ms, at);
    }
    assert_eq!(
        (step(&application, ids[0]), plans(&application)),
        (FoldStep::Peek, 1),
        "a promoted Command streaming on plans the Transcript no more"
    );
}

#[test]
fn an_aged_command_in_a_collapsed_group_grows_once_the_group_opens() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[
        ("command 1", ActivityStatus::Completed),
        ("command 2", ActivityStatus::Completed),
        ("command 3", ActivityStatus::Active),
    ]);
    let mut application = application_with(&elapsed_ms, 0, GroupPosture::Collapsed, snapshot);

    for at in [0, 32, 64, 96] {
        tick_at(&mut application, &elapsed_ms, at);
    }
    assert_eq!(
        (step(&application, ids[2]), plans(&application)),
        (FoldStep::Folded, 1),
        "a Command hidden in a collapsed Group stays folded, and waiting plans only once"
    );

    let reference = application
        .state
        .session_reference
        .clone()
        .expect("a Session is open");
    application
        .state
        .session_interaction(&reference)
        .expect("the open Session has view state")
        .groups
        .borrow_mut()
        .expand(ids[0]);
    tick_at(&mut application, &elapsed_ms, 128);
    assert_eq!(
        (step(&application, ids[2]), plans(&application)),
        (FoldStep::Peek, 2),
        "opening the Group lets the next tick see the Command and grow it"
    );

    tick_at(&mut application, &elapsed_ms, 160);
    assert_eq!(
        plans(&application),
        2,
        "once grown, the Command asks for no further plan"
    );
}

#[test]
fn a_reader_folding_a_promoted_command_keeps_it_folded() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[("cargo test", ActivityStatus::Active)]);
    let mut application = application_with(&elapsed_ms, 0, GroupPosture::Off, snapshot);
    tick_at(&mut application, &elapsed_ms, 0);
    assert_eq!(step(&application, ids[0]), FoldStep::Peek);

    let reference = application
        .state
        .session_reference
        .clone()
        .expect("a Session is open");
    application
        .state
        .session_interaction(&reference)
        .expect("the open Session has view state")
        .folds
        .borrow_mut()
        .fold(ids[0]);
    for at in [32, 64, 96] {
        tick_at(&mut application, &elapsed_ms, at);
    }
    assert_eq!(
        (step(&application, ids[0]), plans(&application)),
        (FoldStep::Folded, 1),
        "the reader's Fold outlasts later ticks, which plan nothing for it"
    );
}

#[test]
fn a_second_command_aging_grows_beside_the_first() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[("cargo build", ActivityStatus::Active)]);
    let turn_id = snapshot.turns[0].id;
    let mut application = application_with(&elapsed_ms, 500, GroupPosture::Off, snapshot);
    tick_at(&mut application, &elapsed_ms, 500);
    assert_eq!(step(&application, ids[0]), FoldStep::Peek);

    elapsed_ms.store(600, Ordering::Relaxed);
    // A command arrives as it begins, with nothing printed yet.
    let mut second = command(turn_id, "cargo test", ActivityStatus::Active);
    if let Activity::Command { output, .. } = &mut second {
        output.clear();
    }
    let second_id = second.id();
    update(
        &mut application,
        SessionChange::ActivityAdded { activity: second },
    );
    tick_at(&mut application, &elapsed_ms, 1_099);
    assert_eq!(
        (step(&application, second_id), plans(&application)),
        (FoldStep::Folded, 1),
        "the second Command waits out its own latency without a plan"
    );

    tick_at(&mut application, &elapsed_ms, 1_100);
    assert_eq!(
        (
            step(&application, ids[0]),
            step(&application, second_id),
            plans(&application)
        ),
        (FoldStep::Peek, FoldStep::Peek, 2),
        "the second Command grows when it ages, and the first stays grown"
    );

    tick_at(&mut application, &elapsed_ms, 1_132);
    assert_eq!(plans(&application), 2, "both grown, no tick plans again");
}

#[test]
fn an_equal_revision_snapshot_replacement_is_read_afresh() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[
        ("command 1", ActivityStatus::Completed),
        ("cargo build", ActivityStatus::Active),
    ]);
    let turn_id = snapshot.turns[0].id;
    let mut application =
        application_with(&elapsed_ms, 0, GroupPosture::Collapsed, snapshot.clone());
    // The reader holds open a Group this reading of the Session has no
    // leader for, so the Command waits in a collapsed one.
    let leader = command(turn_id, "command 0", ActivityStatus::Completed);
    open_group(&application, leader.id());
    tick_at(&mut application, &elapsed_ms, 0);
    assert_eq!(
        (step(&application, ids[1]), plans(&application)),
        (FoldStep::Folded, 1),
        "the Command waits hidden in a collapsed Group"
    );

    // A fresh snapshot at the same revision — as after a reconnect — whose
    // run the open Group now leads.
    let mut activities = snapshot.activities.clone();
    activities.insert(0, leader);
    application
        .handle_event(ApplicationEvent::SessionAttached(replaced(
            &snapshot, activities,
        )))
        .expect("replace the Session's snapshot");
    tick_at(&mut application, &elapsed_ms, 32);
    assert_eq!(
        (step(&application, ids[1]), plans(&application)),
        (FoldStep::Peek, 2),
        "the replacement is planned afresh, and the Command it shows grows"
    );
}

#[test]
fn the_same_session_on_another_origin_is_read_afresh() {
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let (snapshot, ids) = session_running(&[
        ("command 1", ActivityStatus::Completed),
        ("cargo build", ActivityStatus::Active),
    ]);
    let turn_id = snapshot.turns[0].id;
    let mut application =
        application_with(&elapsed_ms, 0, GroupPosture::Collapsed, snapshot.clone());
    let leader = command(turn_id, "command 0", ActivityStatus::Completed);
    let leader_id = leader.id();
    open_group(&application, leader_id);
    tick_at(&mut application, &elapsed_ms, 0);
    assert_eq!(
        (step(&application, ids[1]), plans(&application)),
        (FoldStep::Folded, 1),
        "the Command waits hidden in a collapsed Group"
    );

    // Another Origin holds a Session of the same id at the same revision,
    // running a different Command in a run the same Group leads, and the
    // reader holds that Group open there too.
    application.state.outlook = Outlook::Remote("elsewhere".to_owned());
    let elsewhere = command(turn_id, "cargo test", ActivityStatus::Active);
    let elsewhere_id = elsewhere.id();
    application
        .handle_event(ApplicationEvent::SessionAttached(replaced(
            &snapshot,
            vec![leader, elsewhere],
        )))
        .expect("open the same Session id on another Origin");
    open_group(&application, leader_id);
    tick_at(&mut application, &elapsed_ms, 32);
    assert_eq!(
        (step(&application, elsewhere_id), plans(&application)),
        (FoldStep::Peek, 2),
        "the other Origin's Session is planned afresh, and its Command grows"
    );
}
