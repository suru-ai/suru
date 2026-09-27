//! The Aside among the surfaces around it: overlays opened over the main view,
//! the Intervention panels that outrank it for the keys, and the commands that
//! move its edge a column at a time.

use crate::support::{
    add_activity, approval_activity, connected_application, deliver_settings, enter_active_session,
    invoke, key, rendered_application_buffer, rendered_application_rows_at, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use std::time::Duration;
use suru::{
    managed_client::{SessionEvent, SubagentTreeEvent},
    protocol::{
        ActivityStatus, ApprovalOutcome, ApprovalSubject, AsideSettings, AsideVisibility,
        EffectiveSettings, Outlook, SessionId, SessionReference, SessionSnapshot,
        SidebarVisibility, SubagentTreeEntry, SubagentTreeRevision, SubagentTreeSnapshot,
        SubagentTreeTopLevel, TurnId,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 24;

/// A client whose Settings show the Aside at `aside_width` and keep the
/// Sidebar off the frame, with an Intervention panel that takes keys the
/// moment it presents itself.
fn client(workspace: &std::path::Path, aside_width: u64) -> Application {
    let mut application =
        connected_application(workspace).with_intervention_arming_delay(Duration::ZERO);
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.aside = AsideSettings {
        initial_visibility: AsideVisibility::Shown,
        initial_width: aside_width,
    };
    deliver_settings(&mut application, settings);
    application
}

/// An open, Working top-level Session with one Subagent, its tree in hand.
fn session_with_its_tree(
    application: &mut Application,
    workspace: &std::path::Path,
) -> (SessionSnapshot, TurnId) {
    let (session_id, snapshot, turn_id) = enter_active_session(application, workspace);
    application
        .handle_event(ApplicationEvent::SubagentTree {
            through: SessionReference::new(Outlook::Local, session_id),
            event: SubagentTreeEvent::Snapshot(SubagentTreeSnapshot {
                revision: SubagentTreeRevision::INITIAL,
                top_level: SubagentTreeTopLevel {
                    session_id,
                    title: "Map every seam".to_owned(),
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
                },
                subagents: vec![SubagentTreeEntry {
                    session_id: SessionId::new(),
                    parent_session_id: session_id,
                    spawn_order: 0,
                    name: "Explore".to_owned(),
                    title: "Map the provider seams".to_owned(),
                    model: None,
                    status: ActivityStatus::Completed,
                    worked_ms: Some(12_000),
                    working_since: None,
                    monitoring_since: None,
                    needs_intervention: false,
                }],
            }),
        })
        .expect("take the Session's tree");
    (snapshot, turn_id)
}

/// The Aside's columns, rule included, of every rendered row.
fn aside_columns(application: &Application, aside_width: u16) -> Vec<String> {
    rendered_application_rows_at(application, WIDTH, HEIGHT)
        .iter()
        .map(|row| row.chars().skip(usize::from(WIDTH - aside_width)).collect())
        .collect()
}

/// The Aside rows painted with row focus.
fn focused_aside_rows(application: &Application) -> Vec<u16> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    (0..HEIGHT)
        .filter(|row| buffer[(WIDTH - 30, *row)].bg == Color::Blue)
        .collect()
}

fn rule_column(application: &Application, width: u16) -> Option<usize> {
    let rows = rendered_application_rows_at(application, width, HEIGHT);
    rows[0]
        .chars()
        .position(|character| character == '│')
        .filter(|_| rows.join("\n").contains("Subagents"))
}

fn press(application: &mut Application, code: KeyCode, modifiers: KeyModifiers) {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("press a key");
}

fn press_leader_chord(application: &mut Application, key: char) {
    press(application, KeyCode::Char('x'), KeyModifiers::CONTROL);
    press(application, KeyCode::Char(key), KeyModifiers::NONE);
}

/// Adds one pending Approval to the open Session and delivers it.
fn an_approval_arrives(
    application: &mut Application,
    snapshot: &mut SessionSnapshot,
    turn_id: TurnId,
) {
    let activity = approval_activity(
        turn_id,
        ApprovalSubject::Network {
            host_or_url: "https://api.example.test/v1".to_owned(),
        },
        None,
        ApprovalOutcome::Pending,
        None,
    );
    let suru::protocol::Activity::Approval { approval, .. } = &activity else {
        unreachable!("an Approval Activity carries an Approval")
    };
    snapshot.pending_approvals.push(approval.id);
    snapshot.revision = suru::protocol::SessionRevision(snapshot.revision.0 + 1);
    snapshot.pending_approvals_revision = snapshot.revision;
    add_activity(snapshot, activity);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .expect("deliver the pending Approval");
}

fn screen(application: &Application) -> String {
    rendered_application_rows_at(application, WIDTH, HEIGHT).join("\n")
}

#[test]
fn an_overlay_opens_over_the_main_view_and_never_over_the_aside() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path(), 32);
    session_with_its_tree(&mut application, workspace.path());
    let before = aside_columns(&application, 32);
    assert!(before[0].starts_with('│') && before.join("\n").contains("Subagents 1"));

    for overlay in [
        SemanticCommandId::SessionList,
        SemanticCommandId::SettingsOpen,
    ] {
        invoke(&mut application, overlay);
        assert!(
            rendered_application_rows_at(&application, WIDTH, HEIGHT)
                .join("\n")
                .contains(if overlay == SemanticCommandId::SessionList {
                    "Sessions"
                } else {
                    "Settings"
                }),
            "the overlay is on screen"
        );
        assert_eq!(
            aside_columns(&application, 32),
            before,
            "{overlay:?} is centred on the main view and leaves every cell of the Aside standing"
        );
        key(&mut application, KeyCode::Esc);
    }
}

#[test]
fn an_open_intervention_panel_outranks_an_aside_holding_the_keys_until_it_closes() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path(), 32);
    let (mut snapshot, turn_id) = session_with_its_tree(&mut application, workspace.path());
    an_approval_arrives(&mut application, &mut snapshot, turn_id);
    assert!(
        screen(&application).contains("Approval · Choose Decision"),
        "the Approval presents itself"
    );

    // The Leader is reachable from the panel, so the reader can give the
    // Aside the keys while it stands.
    press_leader_chord(&mut application, 'a');
    assert!(
        focused_aside_rows(&application).is_empty(),
        "the panel outranks the Aside, so no row focus is painted beneath it"
    );
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert!(
        screen(&application).contains("> 1. Accept once"),
        "the keys go to the Intervention: {}",
        screen(&application)
    );
    assert!(focused_aside_rows(&application).is_empty());

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!screen(&application).contains("Approval · Choose Decision"));
    assert_eq!(
        focused_aside_rows(&application),
        [1],
        "with the panel put away the Aside has the keys it was given, focus on the open entry"
    );
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        focused_aside_rows(&application),
        [2, 3],
        "and they walk its entries, a Subagent's two lines painted as one"
    );
}

#[test]
fn an_intervention_arriving_while_the_aside_holds_the_keys_waits_for_them() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path(), 32);
    let (mut snapshot, turn_id) = session_with_its_tree(&mut application, workspace.path());
    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(focused_aside_rows(&application), [1]);

    an_approval_arrives(&mut application, &mut snapshot, turn_id);
    assert!(
        !screen(&application).contains("Approval · Choose Decision"),
        "an arrival does not take the keys from a column the reader is driving, as with the Sidebar"
    );
    assert_eq!(focused_aside_rows(&application), [1]);

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        screen(&application).contains("Approval · Choose Decision"),
        "handing the keys back lets the waiting Approval present itself"
    );
    assert!(focused_aside_rows(&application).is_empty());
}

#[test]
fn semantic_commands_widen_and_narrow_the_drawn_aside_one_column() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path(), 40);
    session_with_its_tree(&mut application, workspace.path());
    assert_eq!(
        rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 40))
    );

    assert_eq!(
        invoke(&mut application, SemanticCommandId::AsideWiden),
        ApplicationTransition::Continue
    );
    assert_eq!(
        rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 41)),
        "widening moves the edge a column toward the main view"
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::AsideNarrow),
        ApplicationTransition::Continue
    );
    assert_eq!(
        rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 40))
    );
}

#[test]
fn incremental_width_commands_stop_at_the_aside_and_main_view_floors() {
    let workspace = workspace_dir();
    let mut minimum = client(workspace.path(), 24);
    session_with_its_tree(&mut minimum, workspace.path());
    assert_eq!(rule_column(&minimum, WIDTH), Some(usize::from(WIDTH - 24)));
    invoke(&mut minimum, SemanticCommandId::AsideNarrow);
    assert_eq!(
        rule_column(&minimum, WIDTH),
        Some(usize::from(WIDTH - 24)),
        "the Aside's own floor"
    );

    let mut maximum = client(workspace.path(), 46);
    session_with_its_tree(&mut maximum, workspace.path());
    assert_eq!(rule_column(&maximum, 100), Some(54));
    invoke(&mut maximum, SemanticCommandId::AsideWiden);
    assert_eq!(
        rule_column(&maximum, 100),
        Some(54),
        "the main view's floor, at the width the last frame drew"
    );
    assert_eq!(
        rule_column(&maximum, WIDTH),
        Some(usize::from(WIDTH - 46)),
        "and widening at that boundary left no latent choice behind"
    );

    let mut hidden = client(workspace.path(), 40);
    session_with_its_tree(&mut hidden, workspace.path());
    invoke(&mut hidden, SemanticCommandId::AsideToggle);
    invoke(&mut hidden, SemanticCommandId::AsideToggle);
    assert_eq!(rule_column(&hidden, WIDTH), None);
    invoke(&mut hidden, SemanticCommandId::AsideWiden);
    invoke(&mut hidden, SemanticCommandId::AsideToggle);
    assert_eq!(
        rule_column(&hidden, WIDTH),
        Some(usize::from(WIDTH - 40)),
        "a hidden Aside is not resized"
    );
}
