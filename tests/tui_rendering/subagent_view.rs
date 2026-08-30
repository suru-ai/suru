//! The way into a Subagent's Session and back: pressing its Transcript row
//! opens the child in the Session Content Column, and Escape returns to the
//! parent exactly where the reader left it. The child view offers no path to
//! delivering a Prompt.

use crate::support::{
    buffer_rows, connected_application, failed_session_snapshot, navigable_session_snapshot,
    rendered_application_buffer, rendered_application_rows_at, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityStatus, Cost, CostBasis, Message, MessageId, MessageRole, MessageStatus,
        ModelAvailability, PromptId, Session, SessionChange, SessionId, SessionRevision,
        SessionSnapshot, SessionStatus, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus,
        Usage, UsageTotal, Workspace,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition},
};

/// A parent Session whose Transcript carries one Subagent row, answering with
/// the child Session the row names.
fn parent_with_subagent_row(
    workspace: &std::path::Path,
    status: ActivityStatus,
    duration_ms: Option<u64>,
    turn_in_flight: bool,
) -> (SessionSnapshot, SessionId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the mapping",
        workspace,
    );
    let turn_id = snapshot.turns[0].id;
    if turn_in_flight {
        snapshot.session.status = SessionStatus::Active;
        snapshot.turns[0].status = TurnStatus::Active;
    } else {
        snapshot.turns[0].status = TurnStatus::Completed;
    }
    let child_id = SessionId::new();
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Subagent {
        id: activity_id,
        turn_id,
        status,
        name: "Explore".to_owned(),
        description: "Map the provider seams".to_owned(),
        session_id: child_id,
        duration_ms,
    };
    (snapshot, child_id)
}

/// A Subagent's own Session: parented, running its prompt-less Turn, with the
/// Subagent's narration in its Transcript.
fn child_session_snapshot(
    child_id: SessionId,
    parent_id: SessionId,
    workspace: &std::path::Path,
) -> SessionSnapshot {
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    SessionSnapshot {
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
            usage: None,
            cost: None,
            cost_basis: None,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "Mapping the provider seams".to_owned(),
            truncated: false,
            skill_invocations: Vec::new(),
        }],
        activities: Vec::new(),
        transcript: vec![TranscriptItem::Message { message_id }],
        subagent_usage: None,
    }
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

fn press_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press the key")
}

#[test]
fn pressing_a_working_subagent_row_opens_the_childs_session() {
    let workspace = workspace_dir();
    let (snapshot, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, true);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");

    assert_eq!(
        press_text(&mut application, 80, 22, "Explore: Map the provider seams"),
        ApplicationTransition::AttachSession(child_id),
        "pressing the row asks to open the child Session it names"
    );
}

#[test]
fn a_settled_subagent_row_in_history_still_opens_the_child() {
    let workspace = workspace_dir();
    let (snapshot, child_id) = parent_with_subagent_row(
        workspace.path(),
        ActivityStatus::Completed,
        Some(12_000),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Subagent settled");
    // The settled Turn folds to its marker, so the reader opens the Turn
    // Folds before the row is theirs to press.
    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("open the settled Turn Folds");
    }

    assert_eq!(
        press_text(&mut application, 80, 22, "Explore: Map the provider seams"),
        ApplicationTransition::AttachSession(child_id),
        "a settled row in scrollback is still the way into the child"
    );
}

#[test]
fn escape_in_a_subagent_session_returns_to_the_parent() {
    let workspace = workspace_dir();
    let parent_id = SessionId::new();
    let child = child_session_snapshot(SessionId::new(), parent_id, workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");
    rendered_application_rows_at(&application, 80, 22);

    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::AttachSession(parent_id),
        "Escape asks to open the parent Session, never to interrupt the child"
    );
}

#[test]
fn returning_to_the_parent_restores_the_readers_view_state() {
    let workspace = workspace_dir();
    let parent_id = SessionId::new();
    let parent = navigable_session_snapshot(parent_id, workspace.path(), 8);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(parent.clone()))
        .expect("attach the parent Session");
    rendered_application_rows_at(&application, 80, 15);
    for _ in 0..3 {
        application
            .handle_terminal_event(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 40,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }))
            .expect("scroll the parent's Transcript up");
        rendered_application_rows_at(&application, 80, 15);
    }
    let left_at = rendered_application_rows_at(&application, 80, 15);

    let child = child_session_snapshot(SessionId::new(), parent_id, workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("open the Subagent's Session");
    rendered_application_rows_at(&application, 80, 15);
    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::AttachSession(parent_id),
        "Escape asks for the parent back"
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("return to the parent Session");

    assert_eq!(
        rendered_application_rows_at(&application, 80, 15),
        left_at,
        "the parent comes back exactly where the reader left it"
    );
}

#[test]
fn a_subagent_sessions_view_states_its_own_total_where_its_parents_carries_the_child_too() {
    let workspace = workspace_dir();
    let (mut parent, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, true);
    let parent_id = parent.session.id;
    parent.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(10_000),
        output_tokens: Some(5_000),
        ..Usage::default()
    });
    parent.turns[0].cost = Cost::from_usd(0.31);
    parent.turns[0].cost_basis = Some(CostBasis::Reported);
    parent.subagent_usage = Some(UsageTotal {
        fresh_input_tokens: Some(4_000),
        output_tokens: Some(1_000),
        cost: Cost::from_usd(0.12),
        ..UsageTotal::default()
    });
    let mut child = child_session_snapshot(child_id, parent_id, workspace.path());
    child.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(4_000),
        output_tokens: Some(1_000),
        ..Usage::default()
    });
    child.turns[0].cost = Cost::from_usd(0.12);
    child.turns[0].cost_basis = Some(CostBasis::Reported);

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("attach the delegating Session");
    let delegating = buffer_rows(&rendered_application_buffer(&application, 80, 22)).join("\n");
    assert!(
        delegating.contains("20K · $0.43"),
        "the parent states its own work and the Subagent's together: {delegating}"
    );

    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("open the Subagent's Session");
    let delegated = buffer_rows(&rendered_application_buffer(&application, 80, 22)).join("\n");
    assert!(
        delegated.contains("5K · $0.12"),
        "the child states what it consumed itself: {delegated}"
    );
    assert!(
        !delegated.contains("20K"),
        "and never what its parent consumed: {delegated}"
    );
}

#[test]
fn a_subagent_session_offers_no_path_to_a_prompt() {
    let workspace = workspace_dir();
    let child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");
    rendered_application_rows_at(&application, 80, 22);

    type_terminal_text(&mut application, "hello agent");
    assert_eq!(
        press_key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "Enter delivers nothing from a Subagent's Session"
    );

    let buffer = rendered_application_buffer(&application, 80, 22);
    let text = buffer_rows(&buffer).join("\n");
    assert!(
        !text.contains("hello agent"),
        "typed text lands nowhere — there is no composer to hold it: {text}"
    );
    assert!(
        text.contains("Esc returns to the parent"),
        "the view says how to leave instead of offering a composer: {text}"
    );
    assert!(
        !text.contains("Esc interrupt"),
        "Escape no longer reads as the interrupt gesture: {text}"
    );
}

#[test]
fn a_subagent_session_streams_live_while_attached() {
    let workspace = workspace_dir();
    let child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    let child_id = child.session.id;
    let turn_id = child.turns[0].id;
    let revision = child.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id: child_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::Agent,
                        status: MessageStatus::Completed,
                        content: "Found the orchestration seam".to_owned(),
                        truncated: false,
                        skill_invocations: Vec::new(),
                    },
                }],
            },
        )))
        .expect("stream the Subagent's next message");

    let rows = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        rows.contains("Found the orchestration seam"),
        "the child's Transcript streams live while the Subagent works: {rows}"
    );
}
