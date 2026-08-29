//! The Subagent Picker: pressing Down while the open Session has working
//! Subagents docks the picker over the composer, drawn as the tree they
//! spawned in and updated live as they spawn and settle. Arrows move through
//! it, Enter opens the chosen Subagent's Session, the pointer answers as
//! readily as the keys, and closing lands back where it opened. With nothing
//! to browse the key stays inert and composer history keeps its meaning.

use crate::support::{
    connected_application, enter_session, failed_session_snapshot, rendered_application_buffer,
    rendered_application_rows_at, text_position, type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
        ModelAvailability, PromptId, Session, SessionChange, SessionId, SessionRevision,
        SessionSnapshot, SessionStatus, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus,
        Workspace,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition},
};

/// One working Subagent a fixture Session spawned: the child Session its row
/// names, and the Activity carrying its lifecycle.
struct SpawnedSubagent {
    child_id: SessionId,
    activity_id: ActivityId,
}

/// A parent Session whose active Turn spawned one working Subagent per given
/// name and description.
fn parent_with_working_subagents(
    workspace: &std::path::Path,
    subagents: &[(&str, &str)],
) -> (SessionSnapshot, Vec<SpawnedSubagent>) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the mapping",
        workspace,
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let turn_id = snapshot.turns[0].id;
    let mut spawned = Vec::new();
    for (index, (name, description)) in subagents.iter().enumerate() {
        let child_id = SessionId::new();
        let activity_id = if index == 0 {
            snapshot.activities[0].id()
        } else {
            ActivityId::new()
        };
        let activity = Activity::Subagent {
            id: activity_id,
            turn_id,
            status: ActivityStatus::Active,
            name: (*name).to_owned(),
            description: (*description).to_owned(),
            session_id: child_id,
            duration_ms: None,
        };
        if index == 0 {
            snapshot.activities[0] = activity;
        } else {
            snapshot.activities.push(activity);
            snapshot
                .transcript
                .push(TranscriptItem::Activity { activity_id });
        }
        spawned.push(SpawnedSubagent {
            child_id,
            activity_id,
        });
    }
    (snapshot, spawned)
}

/// A Subagent's own Session whose prompt-less Turn spawned a working Subagent
/// of its own, answering with the grandchild's Session.
fn child_with_working_subagent(
    child_id: SessionId,
    parent_id: SessionId,
    workspace: &std::path::Path,
) -> (SessionSnapshot, SessionId) {
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    let grandchild_id = SessionId::new();
    let activity_id = ActivityId::new();
    let snapshot = SessionSnapshot {
        session: Session {
            id: child_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Active,
            parent: Some(parent_id),
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: vec![Turn {
            id: turn_id,
            prompt_id: None,
            agent: None,
            status: TurnStatus::Active,
            started_at: None,
            settled_at: None,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "Delegating one level down".to_owned(),
            truncated: false,
            skill_invocations: Vec::new(),
        }],
        activities: vec![Activity::Subagent {
            id: activity_id,
            turn_id,
            status: ActivityStatus::Active,
            name: "Verify".to_owned(),
            description: "Check the mapped seams".to_owned(),
            session_id: grandchild_id,
            duration_ms: None,
        }],
        transcript: vec![
            TranscriptItem::Message { message_id },
            TranscriptItem::Activity { activity_id },
        ],
    };
    (snapshot, grandchild_id)
}

fn press_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press the key")
}

/// Presses the primary pointer button on the first rendered occurrence of
/// `needle`, the way a reader clicks a row they can see.
fn press_text(
    application: &mut Application,
    width: u16,
    height: u16,
    needle: &str,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, width, height);
    let (column, row) = text_position(&buffer, needle);
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("press the rendered row")
}

/// Settles one Subagent through the live session stream, the way the server
/// reports it.
fn settle_subagent(
    application: &mut Application,
    session_id: SessionId,
    revision: u64,
    activity_id: ActivityId,
) {
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision),
                changes: vec![SessionChange::SubagentStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(12_000),
                }],
            },
        )))
        .expect("settle the Subagent over the session stream");
}

#[test]
fn down_opens_the_picker_while_subagents_work() {
    let workspace = workspace_dir();
    let (snapshot, _) = parent_with_working_subagents(
        workspace.path(),
        &[
            ("Explore", "Map the provider seams"),
            ("Plan", "Design the picker"),
        ],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with working Subagents");

    assert_eq!(
        press_key(&mut application, KeyCode::Down),
        ApplicationTransition::Continue,
        "opening the picker is view state, not a server conversation"
    );

    let rows = rendered_application_rows_at(&application, 80, 22);
    let text = rows.join("\n");
    assert!(
        text.contains("Subagents"),
        "the picker docks over the composer under its own title: {text}"
    );
    assert!(
        text.contains("├ ⠋ Explore: Map the provider seams")
            && text.contains("└ ⠋ Plan: Design the picker"),
        "every working Subagent stands in the tree it spawned in: {text}"
    );
}

#[test]
fn down_stays_inert_without_working_subagents() {
    let workspace = workspace_dir();
    let (mut snapshot, spawned) =
        parent_with_working_subagents(workspace.path(), &[("Explore", "Map the provider seams")]);
    // The one Subagent already settled, so there is nothing to browse.
    snapshot.session.status = SessionStatus::Idle;
    snapshot.turns[0].status = TurnStatus::Completed;
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = &mut snapshot.activities[0]
    else {
        panic!("the fixture's first Activity is the Subagent row");
    };
    *status = ActivityStatus::Completed;
    *duration_ms = Some(12_000);
    drop(spawned);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Subagent settled");
    let before = rendered_application_rows_at(&application, 80, 22);

    assert_eq!(
        press_key(&mut application, KeyCode::Down),
        ApplicationTransition::Continue,
        "an inert key changes nothing"
    );
    assert_eq!(
        rendered_application_rows_at(&application, 80, 22),
        before,
        "with nothing to browse, Down leaves the frame exactly as it stood"
    );
}

#[test]
fn down_moves_the_caret_before_it_opens_the_picker() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        parent_with_working_subagents(workspace.path(), &[("Explore", "Map the provider seams")]);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");
    type_terminal_text(&mut application, "first line");
    application
        .handle_event(ApplicationEvent::Command(
            suru::tui::CommandId::InsertNewline,
        ))
        .expect("break the draft onto a second line");
    type_terminal_text(&mut application, "second line");
    // Up moves the caret into the first line; the next Down is caret movement
    // back to the last line, not the picker.
    press_key(&mut application, KeyCode::Up);
    press_key(&mut application, KeyCode::Down);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        !text.contains("Subagents"),
        "Down within the draft keeps its caret meaning: {text}"
    );

    // At rest on the last line, Down has no composer meaning left to keep.
    press_key(&mut application, KeyCode::Down);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("└ ⠋ Explore: Map the provider seams"),
        "Down at rest opens the picker: {text}"
    );
}

#[test]
fn down_keeps_walking_history_even_while_subagents_work() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let (session_id, snapshot) = enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::ActivityAdded {
                    activity: Activity::Subagent {
                        id: ActivityId::new(),
                        turn_id: snapshot.turns[0].id,
                        status: ActivityStatus::Active,
                        name: "Explore".to_owned(),
                        description: "Map the provider seams".to_owned(),
                        session_id: SessionId::new(),
                        duration_ms: None,
                    },
                }],
            },
        )))
        .expect("spawn a working Subagent over the session stream");

    // Up recalls the submitted Prompt, so a history walk is in progress.
    press_key(&mut application, KeyCode::Up);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("│Initial Prompt"),
        "Up recalls the submitted Prompt into the composer: {text}"
    );

    // Down keeps stepping the walk — here, back off its end to the scratch —
    // rather than opening the picker over the reader's navigation.
    press_key(&mut application, KeyCode::Down);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        !text.contains("│Initial Prompt"),
        "Down steps the history walk back to the empty scratch: {text}"
    );
    assert!(
        !text.contains("Subagents"),
        "composer history keeps its meaning while it has one: {text}"
    );

    // With the walk over, Down is back to its one free meaning.
    press_key(&mut application, KeyCode::Down);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("└ ⠋ Explore: Map the provider seams"),
        "Down at rest opens the picker: {text}"
    );
}

#[test]
fn the_picker_takes_in_a_subagent_spawning_while_it_is_open() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        parent_with_working_subagents(workspace.path(), &[("Explore", "Map the provider seams")]);
    let session_id = snapshot.session.id;
    let revision = snapshot.revision.0;
    let turn_id = snapshot.turns[0].id;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");
    press_key(&mut application, KeyCode::Down);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision + 1),
                changes: vec![SessionChange::ActivityAdded {
                    activity: Activity::Subagent {
                        id: ActivityId::new(),
                        turn_id,
                        status: ActivityStatus::Active,
                        name: "Plan".to_owned(),
                        description: "Design the picker".to_owned(),
                        session_id: SessionId::new(),
                        duration_ms: None,
                    },
                }],
            },
        )))
        .expect("spawn a second Subagent over the session stream");

    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("├ ⠋ Explore: Map the provider seams")
            && text.contains("└ ⠋ Plan: Design the picker"),
        "a Subagent spawning while the picker is open joins the tree: {text}"
    );
}

#[test]
fn the_picker_updates_live_and_closes_when_the_last_subagent_settles() {
    let workspace = workspace_dir();
    let (snapshot, spawned) = parent_with_working_subagents(
        workspace.path(),
        &[
            ("Explore", "Map the provider seams"),
            ("Plan", "Design the picker"),
        ],
    );
    let session_id = snapshot.session.id;
    let revision = snapshot.revision.0;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with working Subagents");
    press_key(&mut application, KeyCode::Down);

    settle_subagent(
        &mut application,
        session_id,
        revision + 1,
        spawned[0].activity_id,
    );
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("└ ⠋ Plan: Design the picker"),
        "the Subagent still working keeps its entry, now the tree's last: {text}"
    );
    assert_eq!(
        text.matches("Explore: Map the provider seams").count(),
        1,
        "a settled Subagent leaves the picker — only its Transcript row remains: {text}"
    );

    settle_subagent(
        &mut application,
        session_id,
        revision + 2,
        spawned[1].activity_id,
    );
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        !text.contains("Subagents"),
        "the picker never stands over nothing to browse: {text}"
    );
}

#[test]
fn arrows_choose_and_enter_opens_the_chosen_subagent() {
    let workspace = workspace_dir();
    let (snapshot, spawned) = parent_with_working_subagents(
        workspace.path(),
        &[
            ("Explore", "Map the provider seams"),
            ("Plan", "Design the picker"),
        ],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with working Subagents");
    press_key(&mut application, KeyCode::Down);

    press_key(&mut application, KeyCode::Down);
    assert_eq!(
        press_key(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(spawned[1].child_id),
        "Enter opens the Session of the Subagent the arrows chose"
    );
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        !text.contains("Subagents"),
        "choosing puts the picker away: {text}"
    );
}

#[test]
fn escape_closes_the_picker_and_lands_back_where_it_opened() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        parent_with_working_subagents(workspace.path(), &[("Explore", "Map the provider seams")]);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");
    type_terminal_text(&mut application, "a draft in progress");
    let before = rendered_application_rows_at(&application, 80, 22);

    press_key(&mut application, KeyCode::Down);
    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue,
        "Escape closes the picker without interrupting anything"
    );
    assert_eq!(
        rendered_application_rows_at(&application, 80, 22),
        before,
        "closing lands back exactly where it opened, draft and all"
    );
}

#[test]
fn rows_answer_the_pointer_as_readily_as_the_keys() {
    let workspace = workspace_dir();
    let (snapshot, spawned) = parent_with_working_subagents(
        workspace.path(),
        &[
            ("Explore", "Map the provider seams"),
            ("Plan", "Design the picker"),
        ],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with working Subagents");
    press_key(&mut application, KeyCode::Down);

    assert_eq!(
        press_text(&mut application, 80, 22, "└ ⠋ Plan: Design the picker"),
        ApplicationTransition::AttachSession(spawned[1].child_id),
        "pressing a picker row opens the Subagent it names"
    );
}

#[test]
fn a_press_outside_the_picker_dismisses_it() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        parent_with_working_subagents(workspace.path(), &[("Explore", "Map the provider seams")]);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");
    press_key(&mut application, KeyCode::Down);
    rendered_application_rows_at(&application, 80, 22);

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }))
            .expect("press outside the picker"),
        ApplicationTransition::Continue,
        "a press outside the picker is how a reader dismisses it"
    );
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        !text.contains("Subagents"),
        "the dismissed picker is gone: {text}"
    );
}

#[test]
fn a_subagent_session_browses_its_own_working_subagents() {
    let workspace = workspace_dir();
    let (child, grandchild_id) =
        child_with_working_subagent(SessionId::new(), SessionId::new(), workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session that delegated further");

    press_key(&mut application, KeyCode::Down);
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("└ ⠋ Verify: Check the mapped seams"),
        "the picker browses the open Session's own working Subagents, one level down: {text}"
    );
    assert_eq!(
        press_key(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(grandchild_id),
        "Enter steps into the grandchild's Session"
    );
}
