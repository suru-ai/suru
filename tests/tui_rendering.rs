use chidori::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, SessionEvent,
        SessionSubscription,
    },
    protocol::{
        Activity, ActivityId, ActivityStatus, AgentId, AgentIdentity, CreateSessionRequest,
        FileChange, Health, InitialPrompt, LifecycleState, Message, MessageId, MessageRole,
        MessageStatus, ModelId, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus,
        ProviderId, ServerIdentity, ServerShutdown, Session, SessionChange, SessionId,
        SessionRevision, SessionSnapshot, SessionStatus, SessionSummary, SessionTimestamp,
        SessionUpdate, ShutdownReason, TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    server::{AgentOutput, ServerConfig},
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SessionListRequest,
        SessionListScope, TuiState, command_for_terminal_event, render,
    },
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::{Buffer, Cell},
    layout::Position,
    style::{Color, Modifier},
};
use std::time::Duration;
use uuid::Uuid;

#[path = "support/failing_provider.rs"]
mod failing_provider_support;

use failing_provider_support::spawn_with_failing_provider;

fn rendered_rows(render: impl FnOnce(&mut Frame<'_>)) -> Vec<String> {
    rendered_rows_at(80, 15, render)
}

fn rendered_rows_at(width: u16, height: u16, render: impl FnOnce(&mut Frame<'_>)) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(render)
        .expect("render headless TUI application");
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

fn rendered_state_rows(state: &TuiState) -> Vec<String> {
    rendered_rows(|frame| render(frame, state))
}

fn rendered_application_rows(application: &Application) -> Vec<String> {
    rendered_rows(|frame| application.render(frame))
}

fn rendered_application_rows_at(application: &Application, width: u16, height: u16) -> Vec<String> {
    rendered_rows_at(width, height, |frame| application.render(frame))
}

fn rendered_application_buffer(application: &Application, width: u16, height: u16) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render headless TUI application");
    terminal.backend().buffer().clone()
}

fn rendered_application_cursor_at(application: &Application, width: u16, height: u16) -> Position {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render headless TUI application");
    terminal
        .get_cursor_position()
        .expect("read rendered cursor position")
}

fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect()
}

fn text_position(buffer: &Buffer, needle: &str) -> (u16, u16) {
    for (y, row) in buffer_rows(buffer).into_iter().enumerate() {
        if let Some(byte_offset) = row.find(needle) {
            return (
                row[..byte_offset].chars().count() as u16,
                y.try_into().expect("row fits terminal coordinates"),
            );
        }
    }
    panic!("rendered frame did not contain {needle:?}");
}

fn text_cell<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
    buffer
        .cell(text_position(buffer, needle))
        .expect("rendered text position is inside the buffer")
}

fn rendered_row(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("rendered frame did not contain {needle:?}"))
}

async fn apply_next_session_event(
    application: &mut Application,
    subscription: &mut SessionSubscription,
) -> SessionEvent {
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("Session event arrives")
        .expect("Session stream remains open")
        .expect("Session event is valid");
    application
        .handle_event(ApplicationEvent::Session(event.clone()))
        .expect("apply Session event to headless application");
    event
}

fn ready_health(instance_id: Uuid, pid: u32) -> Health {
    Health::new(
        ServerIdentity {
            instance_id,
            pid,
            protocol_version: 1,
            build_identity: "chidori@test".to_owned(),
        },
        LifecycleState::Ready,
    )
}

fn fixture_instance_id() -> Uuid {
    Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID")
}

fn connected_application(workspace: &std::path::Path) -> Application {
    let instance_id = fixture_instance_id();
    let mut application = Application::new(workspace);
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("connect application");
    application
}

fn type_terminal_text(application: &mut Application, text: &str) {
    for character in text.chars() {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )))
                .expect("type terminal text"),
            ApplicationTransition::Continue
        );
    }
}

fn connected_state(instance_id: Uuid, pid: u32) -> TuiState {
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(ready_health(instance_id, pid)));
    state
}

#[test]
fn headless_application_handles_terminal_and_managed_events_through_the_production_renderer() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut application = Application::default();

    let managed_transition = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("handle connected event");
    assert_eq!(managed_transition, ApplicationTransition::Continue);

    let screen = rendered_application_rows(&application).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Prompt"));
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));

    let insert = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))
    .expect("map terminal input to a semantic command");
    assert_eq!(insert, CommandId::InsertText("q".to_owned()));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(insert))
            .expect("insert printable input"),
        ApplicationTransition::Continue
    );
    let quit = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("map Ctrl+C to a semantic command");
    assert_eq!(quit, CommandId::ClearOrExit);
    let clear_transition = application
        .handle_event(ApplicationEvent::Command(quit))
        .expect("handle terminal command");
    assert_eq!(clear_transition, ApplicationTransition::Continue);
    let terminal_transition = application
        .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
        .expect("handle terminal command after clearing the composer");
    assert_eq!(terminal_transition, ApplicationTransition::Exit);
}

#[test]
fn slash_autocomplete_invokes_new_session_from_a_description_match() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());

    type_terminal_text(&mut application, "/fresh");

    let autocomplete = rendered_application_rows(&application).join("\n");
    assert!(autocomplete.contains("/new"));
    assert!(autocomplete.contains("fresh landing composer"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select canonical slash command"),
        ApplicationTransition::DetachSession
    );
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("What would you like to work on?"));
    assert!(landing.contains("Type a Prompt and press Enter"));
    assert!(!landing.contains("Long-running work"));
    assert!(!landing.contains("/new"));
}

#[test]
fn slash_autocomplete_keeps_the_landing_composer_visible_at_minimum_size() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/n");

    let buffer = rendered_application_buffer(&application, 28, 5);
    assert!(buffer_rows(&buffer).join("\n").contains("/new"));
    let cursor = rendered_application_cursor_at(&application, 28, 5);
    assert_eq!(
        buffer
            .cell(Position::new(cursor.x.saturating_sub(1), cursor.y))
            .expect("cell before the composer cursor is visible")
            .symbol(),
        "n"
    );
}

#[test]
fn new_session_keybinding_defers_creation_until_the_next_prompt() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, old_snapshot, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "discard this draft".to_owned(),
        )))
        .expect("type a Session draft");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
            )))
            .expect("begin semantic leader keybinding"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            )))
            .expect("invoke new Session"),
        ApplicationTransition::DetachSession
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            old_snapshot.clone(),
        )))
        .expect("ignore a queued event from the detached Session");
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("What would you like to work on?"));
    assert!(landing.contains("Type a Prompt and press Enter"));
    assert!(!landing.contains("discard this draft"));
    assert!(!landing.contains("Long-running work"));

    application
        .handle_event(ApplicationEvent::SessionAttached(old_snapshot))
        .expect("reattach the independently addressable active Session");
    let reattached = rendered_application_rows(&application).join("\n");
    assert!(reattached.contains("Long-running work"));
    assert!(reattached.contains("active"));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionNew,
            )))
            .expect("return to landing through the same semantic command"),
        ApplicationTransition::DetachSession
    );

    type_terminal_text(&mut application, "Next Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit the next landing Prompt")
    else {
        panic!("the first Prompt after /new should create a Session");
    };
    assert_eq!(request.prompt.text, "Next Prompt");
    assert_eq!(request.workspace.path, workspace.path());
}

#[test]
fn new_session_releases_a_detached_prompt_after_its_admission_succeeds() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Continue in the old Session".to_owned(),
        )))
        .expect("type an in-flight Prompt");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("begin Prompt admission")
    else {
        panic!("the old Session Prompt should be admitted");
    };
    let admitted_prompt_id = request.prompt.id;

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionNew,
            )))
            .expect("detach while admission remains in flight"),
        ApplicationTransition::DetachSession
    );
    application
        .handle_event(ApplicationEvent::PromptAdmissionSucceeded(
            admitted_prompt_id,
        ))
        .expect("acknowledge the detached Prompt admission");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Start separate work".to_owned(),
        )))
        .expect("type the next landing Prompt");

    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit after detached admission settles")
    else {
        panic!("a settled detached admission must not block landing submission");
    };
    assert_eq!(request.prompt.text, "Start separate work");
}

#[test]
fn dismissed_alias_is_submitted_literally_before_turn_interruption() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "/clear");
    let canonical_result = rendered_application_rows(&application).join("\n");
    assert!(canonical_result.contains("/clear"));
    assert!(canonical_result.contains("/new"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("dismiss autocomplete before interrupting"),
        ApplicationTransition::Continue
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Esc again")
    );

    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit dismissed alias as literal Prompt input")
    else {
        panic!("dismissed slash text should remain a normal Prompt");
    };
    assert_eq!(request.prompt.text, "/clear");
}

#[test]
fn pasted_multiline_and_unmatched_slash_text_remain_literal_prompts() {
    let mut pasted = Application::default();
    pasted
        .handle_terminal_event(InputEvent::Paste("/new".to_owned()))
        .expect("paste slash text");
    let ApplicationTransition::CreateSession(pasted_request) = pasted
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit pasted slash text")
    else {
        panic!("pasted slash text should create a Session Prompt");
    };
    assert_eq!(pasted_request.prompt.text, "/new");

    let mut multiline = Application::default();
    multiline
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/new".to_owned(),
        )))
        .expect("type a slash query");
    multiline
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )))
        .expect("make slash text multiline");
    multiline
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "literal continuation".to_owned(),
        )))
        .expect("type multiline continuation");
    let ApplicationTransition::CreateSession(multiline_request) = multiline
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit multiline slash text")
    else {
        panic!("multiline slash text should create a Session Prompt");
    };
    assert_eq!(multiline_request.prompt.text, "/new\nliteral continuation");

    let mut unmatched = Application::default();
    unmatched
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/zzzz".to_owned(),
        )))
        .expect("type unmatched slash text");
    let ApplicationTransition::CreateSession(unmatched_request) = unmatched
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit unmatched slash text")
    else {
        panic!("unmatched slash text should create a Session Prompt");
    };
    assert_eq!(unmatched_request.prompt.text, "/zzzz");
}

#[test]
fn autocomplete_navigation_and_tab_invoke_the_canonical_alias_target() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/clear".to_owned(),
        )))
        .expect("type slash alias");

    for (code, modifiers) in [
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Char('n'), KeyModifiers::CONTROL),
        (KeyCode::Char('p'), KeyModifiers::CONTROL),
        (KeyCode::Down, KeyModifiers::NONE),
    ] {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
                .expect("navigate autocomplete"),
            ApplicationTransition::Continue
        );
    }
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            )))
            .expect("select the alias result with Tab"),
        ApplicationTransition::DetachSession
    );
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("Type a Prompt and press Enter"));
    assert!(!landing.contains("/clear"));
}

#[test]
fn autocomplete_selection_precedes_an_open_scoped_command_mode() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/new".to_owned(),
        )))
        .expect("type slash command");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("open the leader while autocomplete is visible");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("autocomplete owns Enter before the leader"),
        ApplicationTransition::DetachSession
    );
}

#[test]
fn sessions_command_opens_a_loading_picker_for_the_current_workspace() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    type_terminal_text(&mut application, "/sessions");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/sessions")
    );
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /sessions"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Sessions"));
    assert!(picker.contains("Loading"));
}

#[test]
fn session_picker_orders_marks_focuses_and_wraps_live_sessions() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (current_id, _, _) = enter_active_session(&mut application, workspace.path());
    let newest_id = SessionId::new();
    let oldest_id = SessionId::new();

    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                session_summary(
                    oldest_id,
                    workspace.path(),
                    "Oldest work",
                    SessionStatus::Idle,
                    10,
                ),
                session_summary(
                    newest_id,
                    workspace.path(),
                    "Newest work",
                    SessionStatus::Idle,
                    30,
                ),
                session_summary(
                    current_id,
                    workspace.path(),
                    "Current work",
                    SessionStatus::Active,
                    20,
                ),
            ],
        })
        .expect("hydrate Session picker");

    let rows = rendered_application_rows(&application);
    assert!(rendered_row(&rows, "Newest work") < rendered_row(&rows, "Current work"));
    assert!(rendered_row(&rows, "Current work") < rendered_row(&rows, "Oldest work"));
    let current_row = rows
        .iter()
        .find(|row| row.contains("Current work"))
        .expect("render current Session row");
    assert!(current_row.contains("current"));
    assert!(current_row.contains("active"));
    assert!(current_row.contains('›'));
    for title in ["Newest work", "Current work", "Oldest work"] {
        assert!(
            !rows
                .iter()
                .find(|row| row.contains(title))
                .expect("render Session picker row")
                .contains(workspace.path().to_string_lossy().as_ref())
        );
    }
    assert!(rows.join("\n").contains("ago"));

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .expect("wrap Session picker selection");
    }
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select wrapped Session row"),
        ApplicationTransition::AttachSession(newest_id)
    );
}

#[test]
fn session_attachment_failure_preserves_the_original_until_target_hydration() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (original_id, original_snapshot, _) =
        enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "preserved draft".to_owned(),
        )))
        .expect("type original Session draft");
    let target_id = SessionId::new();

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                20,
            ),
            session_summary(
                target_id,
                workspace.path(),
                "Target Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus target Session");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("begin target attachment"),
        ApplicationTransition::AttachSession(target_id)
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Attaching")
    );

    let refresh_request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::SessionAttachmentFailed(
                "target disappeared".to_owned(),
            ))
            .expect("report failed attachment"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: refresh_request,
            sessions: vec![session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                30,
            )],
        })
        .expect("refresh point-in-time Session status");
    let failed = rendered_application_rows(&application).join("\n");
    assert!(failed.contains("target disappeared"));
    let tiny_failure = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(tiny_failure.contains("Error"));
    assert!(tiny_failure.contains("Original"));
    let short_failure = rendered_application_rows_at(&application, 28, 7).join("\n");
    assert!(short_failure.contains("Error"));
    assert!(short_failure.contains("Original"));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close picker after failed attachment");
    let original = rendered_application_rows(&application).join("\n");
    assert!(original.contains("Long-running work"));
    assert!(original.contains("preserved draft"));

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                30,
            ),
            session_summary(
                target_id,
                workspace.path(),
                "Target Session",
                SessionStatus::Idle,
                20,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus target Session again");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("retry target attachment");
    let target_snapshot = failed_session_snapshot(
        target_id,
        PromptId::new(),
        "Target Session transcript",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(target_snapshot))
        .expect("hydrate target Session");
    let target = rendered_application_rows(&application).join("\n");
    assert!(target.contains("Target Session transcript"));
    assert!(!target.contains("Sessions"));

    application
        .handle_event(ApplicationEvent::SessionAttached(original_snapshot))
        .expect("return to original Session");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("preserved draft")
    );
}

#[test]
fn session_picker_searches_titles_and_remembers_all_workspace_scope() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("open semantic leader");
    let current_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::NONE,
            )))
            .expect("invoke Ctrl+X L"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let gamma_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request.clone(),
            sessions: vec![
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Alpha notes",
                    SessionStatus::Idle,
                    20,
                ),
                session_summary(
                    gamma_id,
                    workspace.path(),
                    "Gamma migration",
                    SessionStatus::Idle,
                    10,
                ),
            ],
        })
        .expect("hydrate current-Workspace Sessions");
    type_terminal_text(&mut application, "gMg");
    let filtered = rendered_application_rows(&application).join("\n");
    assert!(filtered.contains("Gamma migration"));
    assert!(!filtered.contains("Alpha notes"));

    let all_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("toggle all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    let loading = rendered_application_rows(&application).join("\n");
    assert!(loading.contains("All Workspaces"));
    assert!(loading.contains("Loading"));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request,
            sessions: vec![session_summary(
                SessionId::new(),
                workspace.path(),
                "Stale current-Workspace result",
                SessionStatus::Idle,
                40,
            )],
        })
        .expect("ignore stale current-Workspace result");
    let still_loading = rendered_application_rows(&application).join("\n");
    assert!(still_loading.contains("Loading"));
    assert!(!still_loading.contains("Stale current-Workspace result"));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: all_request,
            sessions: vec![session_summary(
                gamma_id,
                std::path::Path::new("/ws-two"),
                "Gamma migration",
                SessionStatus::Idle,
                30,
            )],
        })
        .expect("hydrate all-Workspace Sessions");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/ws-two")
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close Session picker");
    type_terminal_text(&mut application, "/resume");
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke /resume alias"),
        SessionListScope::AllWorkspaces,
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close alias-opened picker");
    type_terminal_text(&mut application, "/continue");
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke /continue alias"),
        SessionListScope::AllWorkspaces,
    );
}

#[test]
fn session_picker_switching_restores_each_transcript_viewport() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let first_id = SessionId::new();
    let first_snapshot = navigable_session_snapshot(first_id, workspace.path(), 8);
    let second_id = SessionId::new();
    let second_snapshot = navigable_session_snapshot(second_id, workspace.path(), 3);
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(first_snapshot.clone()))
        .expect("attach first Session");
    rendered_application_rows_at(&application, 72, 18);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into first Session history");
    let first_history = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(first_history.contains("Latest"));

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                first_id,
                workspace.path(),
                "First Session",
                SessionStatus::Idle,
                20,
            ),
            session_summary(
                second_id,
                workspace.path(),
                "Second Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus second Session");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("select second Session");
    application
        .handle_event(ApplicationEvent::SessionAttached(second_snapshot))
        .expect("hydrate second Session");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Agent section 3")
    );

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                second_id,
                workspace.path(),
                "Second Session",
                SessionStatus::Idle,
                30,
            ),
            session_summary(
                first_id,
                workspace.path(),
                "First Session",
                SessionStatus::Idle,
                20,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus first Session");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("select first Session");
    application
        .handle_event(ApplicationEvent::SessionAttached(first_snapshot))
        .expect("rehydrate first Session");
    let restored = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(restored.contains("Latest"));
    assert!(!restored.contains("Agent section 8"));
}

#[test]
fn session_picker_stays_searchable_at_supported_small_terminal_sizes() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let loading = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(loading.contains("Sessions"));
    assert!(loading.contains("Loading"));

    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Alpha",
                    SessionStatus::Idle,
                    10,
                ),
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Beta",
                    SessionStatus::Active,
                    20,
                ),
            ],
        })
        .expect("hydrate Session picker");
    assert!(
        rendered_application_rows_at(&application, 28, 5)
            .join("\n")
            .contains("Beta")
    );
    type_terminal_text(&mut application, "alp");
    assert!(
        rendered_application_rows_at(&application, 28, 5)
            .join("\n")
            .contains("Alpha")
    );
    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    assert!(narrow.contains("Search: alp"));
    assert!(narrow.contains("Ctrl+A"));
}

#[test]
fn session_picker_scroll_window_keeps_the_current_session_visible() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let current_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            current_id,
            PromptId::new(),
            "Current transcript",
            workspace.path(),
        )))
        .expect("attach current Session");
    let newest_id = SessionId::new();
    let mut sessions = (1..=12)
        .map(|index| {
            session_summary(
                if index == 12 {
                    newest_id
                } else {
                    SessionId::new()
                },
                workspace.path(),
                &format!("Recent Session {index}"),
                SessionStatus::Idle,
                100 + index,
            )
        })
        .collect::<Vec<_>>();
    sessions.push(session_summary(
        current_id,
        workspace.path(),
        "Current Session",
        SessionStatus::Idle,
        1,
    ));
    open_session_picker_with(&mut application, sessions);

    let picker = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(picker.contains("Current Session"));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("wrap from current Session to newest Session");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select wrapped newest Session"),
        ApplicationTransition::AttachSession(newest_id)
    );
}

#[test]
fn session_picker_reserves_required_metadata_before_truncating_long_titles() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let current_id = SessionId::new();
    let mut application = Application::new(workspace.path());
    let mut snapshot = failed_session_snapshot(
        current_id,
        PromptId::new(),
        "Current transcript",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach current active Session");
    let current_request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let all_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("toggle all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request,
            sessions: vec![session_summary(
                SessionId::new(),
                workspace.path(),
                "stale",
                SessionStatus::Idle,
                20,
            )],
        })
        .expect("ignore previous scope result");
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: all_request,
            sessions: vec![session_summary(
                current_id,
                std::path::Path::new("/a/very/long/workspace/path/remote-two"),
                "A deliberately enormous Session title that must yield to metadata",
                SessionStatus::Active,
                10,
            )],
        })
        .expect("hydrate all-Workspace Session");

    let rows = rendered_application_rows_at(&application, 80, 15);
    let row = rows
        .iter()
        .find(|row| row.contains("current"))
        .expect("render current Session metadata");
    assert!(row.contains("active"));
    assert!(row.contains("ago"));
    assert!(row.contains("remote-two"));

    let intermediate_rows = rendered_application_rows_at(&application, 50, 15);
    let intermediate_row = intermediate_rows
        .iter()
        .find(|row| row.contains("current"))
        .expect("render current Session metadata at intermediate width");
    assert!(intermediate_row.contains("active"));
    assert!(intermediate_row.contains("ago"));
    assert!(intermediate_row.contains("-two"));
}

#[test]
fn session_picker_consumes_input_before_hidden_composer_actions() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "do not submit this draft".to_owned(),
        )))
        .expect("type Session draft");
    open_session_picker_with(
        &mut application,
        vec![session_summary(
            session_id,
            workspace.path(),
            "Current Session",
            SessionStatus::Active,
            10,
        )],
    );

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::ALT,
            )))
            .expect("picker consumes hidden queue submission binding"),
        ApplicationTransition::Continue
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close Session picker");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("do not submit this draft")
    );
}

#[test]
fn autocomplete_tracks_the_active_composer_when_a_session_attaches() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/".to_owned(),
        )))
        .expect("open autocomplete on landing");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/new")
    );

    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Existing Session Prompt",
            workspace.path(),
        )))
        .expect("attach an existing Session with an empty composer");
    let attached = rendered_application_rows(&application).join("\n");
    assert!(attached.contains("Existing Session Prompt"));
    assert!(!attached.contains("/new"));
}

#[tokio::test]
async fn headless_application_creates_a_session_and_renders_its_first_turn_through_the_managed_client()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "headless-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "headless-session-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let mut application = Application::new(workspace.path());

    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("What would you like to work on?"));
    assert!(landing.contains("Prompt"));

    for character in "Explain this workspace".chars() {
        let command = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )))
        .expect("map typed character to a semantic command");
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(command))
                .expect("edit landing composer"),
            ApplicationTransition::Continue
        );
    }
    let submit = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("map Enter to submit command");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(submit))
        .expect("submit first Prompt")
    else {
        panic!("first Prompt should request atomic Session creation");
    };
    let created = client
        .create_session(request)
        .await
        .expect("create Session through managed client");
    application
        .handle_event(ApplicationEvent::SessionCreated(created.clone()))
        .expect("transition application after Session creation");
    let mut subscription = client
        .attach_session(created.session.id)
        .await
        .expect("subscribe to created Session");
    let session_event = subscription
        .next()
        .await
        .expect("Session snapshot arrives")
        .expect("Session snapshot is valid");
    let SessionEvent::Snapshot(authoritative) = &session_event else {
        panic!("Session attachment begins with an authoritative snapshot");
    };
    assert_eq!(authoritative.session.id, created.session.id);
    let authoritative = authoritative.clone();
    application
        .handle_event(ApplicationEvent::Session(session_event))
        .expect("hydrate the Session route from its stream");

    let transcript = rendered_application_rows(&application).join("\n");
    assert!(transcript.contains("Explain this workspace"));
    assert!(transcript.contains("No Provider runtime is configured"));
    assert!(
        transcript.contains(
            std::fs::canonicalize(workspace.path())
                .expect("canonicalize expected Workspace")
                .to_string_lossy()
                .as_ref()
        )
    );

    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(authoritative.clone()))
        .expect("attach an independent observer to the Session");
    observer
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Observer-only draft".to_owned(),
        )))
        .expect("edit the observer's local draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep this unsent draft".to_owned(),
        )))
        .expect("edit the client-local Session draft");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            authoritative.clone(),
        )))
        .expect("rehydrate the Session from a fresh snapshot");
    let with_draft = rendered_application_rows(&application).join("\n");
    let observer_with_draft = rendered_application_rows(&observer).join("\n");
    assert!(with_draft.contains("Keep this unsent draft"));
    assert!(!with_draft.contains("Observer-only draft"));
    assert!(observer_with_draft.contains("Observer-only draft"));
    assert!(!observer_with_draft.contains("Keep this unsent draft"));

    let replacement_instance = Uuid::new_v4();
    let original_instance = server.descriptor().instance_id;
    let mut attached_with_landing_draft = Application::new(workspace.path());
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(original_instance, server.descriptor().pid),
        )))
        .expect("connect attached client to original server");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Saved landing draft".to_owned(),
        )))
        .expect("edit separate landing draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::SessionAttached(created.clone()))
        .expect("attach while retaining the landing draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Recovered Session draft".to_owned(),
        )))
        .expect("edit attached Session draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(replacement_instance, 84_848),
        )))
        .expect("connect attached client to replacement");
    let recovered_collision = rendered_application_rows(&attached_with_landing_draft).join("\n");
    assert!(recovered_collision.contains("Recovered Session draft"));
    let previous_draft = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )))
    .expect("map Up to local draft history");
    assert_eq!(previous_draft, CommandId::HistoryPrevious);
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(previous_draft))
        .expect("recall the displaced landing draft");
    assert!(
        rendered_application_rows(&attached_with_landing_draft)
            .join("\n")
            .contains("Saved landing draft")
    );

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: Duration::ZERO,
            },
        )))
        .expect("handle replacement recovery");
    let replacement_transition = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(replacement_instance, 84_848),
        )))
        .expect("handle replacement connection");
    assert_eq!(replacement_transition, ApplicationTransition::SessionEnded);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(created)))
        .expect("ignore a queued event from the ended Session");
    let after_replacement = rendered_application_rows(&application).join("\n");
    assert!(after_replacement.contains("What would you like to work on?"));
    assert!(after_replacement.contains("Keep this unsent draft"));
    assert!(after_replacement.contains("Session ended because the shared server was replaced"));
    assert!(!after_replacement.contains("Explain this workspace"));
    assert!(!after_replacement.contains("No Agent is selected"));

    drop(subscription);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn streamed_agent_markdown_updates_one_unboxed_row_through_the_real_session_stream() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "headless-agent-stream-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "headless-agent-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let mut application = Application::new(workspace.path());
    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }

    let created = client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the stream\nwhile keeping this deliberately long user Message elevated across every wrapped continuation of the transcript block, including its semantic left accent"
                    .to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session");
    assert!(matches!(
        apply_next_session_event(&mut application, &mut subscription).await,
        SessionEvent::Snapshot(_)
    ));

    server
        .session_event_sink()
        .publish(
            session_id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Continue with an active Agent".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Continue with an active Agent".to_owned(),
                    },
                },
            ],
        )
        .expect("start an active Turn for Agent output");
    apply_next_session_event(&mut application, &mut subscription).await;

    let output = server.agent_output();
    output
        .emit(
            session_id,
            AgentOutput::Activity {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: "Reading files".to_owned(),
                },
            },
        )
        .expect("publish status Activity");
    apply_next_session_event(&mut application, &mut subscription).await;

    let message_id = MessageId::new();
    output
        .emit(
            session_id,
            AgentOutput::MessageStarted {
                message_id,
                turn_id,
            },
        )
        .expect("start Agent Message");
    apply_next_session_event(&mut application, &mut subscription).await;
    output
        .emit(
            session_id,
            AgentOutput::MessageDelta {
                message_id,
                content: "# Streamed heading\n\nA *useful* [link](https://example.com) with `inline code`.\n\n- first item\n- second item\n\n```rust\nfn main() {"
                    .to_owned(),
            },
        )
        .expect("publish first Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;

    let partial = rendered_application_buffer(&application, 100, 34);
    assert_eq!(
        buffer_rows(&partial)
            .iter()
            .filter(|row| row.contains("Streamed heading"))
            .count(),
        1,
        "one streamed Message must render once rather than once per chunk"
    );

    output
        .emit(
            session_id,
            AgentOutput::MessageDelta {
                message_id,
                content: "\n\n    println!(\"hi\");\n}\n```\n\n<future>Readable fallback</future>"
                    .to_owned(),
            },
        )
        .expect("publish final Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;
    output
        .emit(session_id, AgentOutput::MessageCompleted { message_id })
        .expect("complete Agent Message");
    apply_next_session_event(&mut application, &mut subscription).await;

    let completed = rendered_application_buffer(&application, 100, 34);
    let rows = buffer_rows(&completed);
    let screen = rows.join("\n");
    for readable in [
        "Streamed heading",
        "A useful link (https://example.com) with inline code.",
        "• first item",
        "• second item",
        "fn main() {",
        "println!(\"hi\");",
        "Readable fallback",
    ] {
        assert!(
            screen.contains(readable),
            "missing rendered Markdown: {readable}"
        );
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Streamed heading"))
            .count(),
        1,
        "completion must preserve the stable Message row"
    );

    let user_row = text_position(&completed, "Explain the stream").1;
    let error_row = text_position(&completed, "Error:").1;
    let status_row = text_position(&completed, "Reading files").1;
    let agent_row = text_position(&completed, "Streamed heading").1;
    let code_start_row = text_position(&completed, "fn main() {").1;
    let code_after_blank_row = text_position(&completed, "println!(\"hi\");").1;
    let user_accent_column = text_position(&completed, "Explain the stream")
        .0
        .saturating_sub(2);
    let user_block_right_edge = completed.area.width.saturating_sub(3);
    assert!(user_row < error_row && error_row < status_row && status_row < agent_row);
    assert_eq!(
        code_after_blank_row,
        code_start_row + 2,
        "fenced code preserves blank lines: {:?}",
        &rows[usize::from(code_start_row)..=usize::from(code_after_blank_row)]
    );
    let accented_user_rows = (user_row..error_row)
        .filter(|row| {
            completed
                .cell((user_accent_column, *row))
                .is_some_and(|cell| cell.symbol() == "┃")
        })
        .collect::<Vec<_>>();
    assert!(
        accented_user_rows.len() >= 3,
        "source and wrapped user lines keep the block accent"
    );
    for row in accented_user_rows {
        assert_eq!(
            completed
                .cell((user_accent_column, row))
                .expect("accent cell")
                .fg,
            Color::Cyan
        );
        assert_eq!(
            completed
                .cell((user_block_right_edge, row))
                .expect("elevated row edge")
                .bg,
            Color::Black,
            "the elevated surface spans the full user block width"
        );
    }
    assert_eq!(text_cell(&completed, "┃").fg, Color::Cyan);
    assert_eq!(text_cell(&completed, "Explain the stream").bg, Color::Black);
    assert_eq!(text_cell(&completed, "Error:").fg, Color::Red);
    assert_eq!(text_cell(&completed, "Reading files").fg, Color::DarkGray);
    assert_eq!(text_cell(&completed, "Streamed heading").fg, Color::Cyan);
    assert!(
        text_cell(&completed, "Streamed heading")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        text_cell(&completed, "useful")
            .modifier
            .contains(Modifier::ITALIC)
    );
    assert_eq!(text_cell(&completed, "link").fg, Color::Blue);
    assert!(
        text_cell(&completed, "link")
            .modifier
            .contains(Modifier::UNDERLINED)
    );
    assert_eq!(text_cell(&completed, "inline code").fg, Color::Yellow);

    drop(subscription);
    drop(client);
    drop(output);
    server.shutdown().await.expect("shut down server");
}

#[test]
fn connecting_view_exposes_connection_state_before_a_snapshot_arrives() {
    let screen = rendered_state_rows(&TuiState::default()).join("\n");

    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Type a Prompt and press Enter"));
    assert!(screen.contains("Connecting to Chidori server..."));
}

#[test]
fn connected_view_centers_the_landing_composer_and_shows_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let state = connected_state(instance_id, 42_424);

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Prompt"));
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn composer_cursor_tracks_empty_unicode_and_multiline_input() {
    let mut application = Application::default();
    let empty = rendered_application_buffer(&application, 80, 15);
    let placeholder = text_position(&empty, "Type a Prompt and press Enter");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(placeholder.0, placeholder.1)
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "a🙂β\nsecond".to_owned(),
        )))
        .expect("type multiline Unicode Prompt");
    let multiline = rendered_application_buffer(&application, 80, 15);
    let second = text_position(&multiline, "second");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(second.0 + 6, second.1)
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("move cursor within the second line");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(second.0 + 5, second.1)
    );

    for _ in 0..7 {
        application
            .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
            .expect("move cursor onto the Unicode first line");
    }
    let first = text_position(&multiline, "a🙂");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(first.0 + 3, first.1),
        "the emoji occupies two terminal cells"
    );
}

#[test]
fn composer_cursor_wraps_at_the_right_edge_and_remains_visible_when_scrolled() {
    let mut wrapped = Application::default();
    wrapped
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "x".repeat(70),
        )))
        .expect("fill the composer's content row");
    let wrapped_buffer = rendered_application_buffer(&wrapped, 80, 30);
    let first = text_position(&wrapped_buffer, "xxxx");
    assert_eq!(prompt_block_height(&buffer_rows(&wrapped_buffer)), 4);
    assert_eq!(
        rendered_application_cursor_at(&wrapped, 80, 30),
        Position::new(first.0, first.1 + 1),
        "an insertion point after a full row belongs at the start of the next row"
    );

    let mut scrolled = Application::default();
    scrolled
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            (1..=20)
                .map(|line| format!("line{line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )))
        .expect("type a Prompt taller than the composer cap");
    let scrolled_buffer = rendered_application_buffer(&scrolled, 80, 30);
    let final_line = text_position(&scrolled_buffer, "line20");
    assert_eq!(
        rendered_application_cursor_at(&scrolled, 80, 30),
        Position::new(final_line.0 + 6, final_line.1)
    );
}

#[test]
fn landing_shell_degrades_by_priority_without_sacrificing_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep the composer usable".to_owned(),
        )))
        .expect("type a landing draft");

    let wide = rendered_application_rows_at(&application, 80, 16).join("\n");
    for content in [
        "Chidori",
        "What would you like to work on?",
        "Keep the composer usable",
        "Agent unavailable",
        "Workspace",
        "Connected",
        "Enter submit",
    ] {
        assert!(
            wide.contains(content),
            "wide landing frame omitted {content:?}"
        );
    }

    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    for core in [
        "Chidori",
        "What would you like to work on?",
        "Keep the composer usable",
        "Agent unavailable",
        "Connected",
    ] {
        assert!(
            narrow.contains(core),
            "narrow landing frame omitted {core:?}"
        );
    }
    for secondary in ["Workspace", "Provider", "Model", "Enter submit"] {
        assert!(
            !narrow.contains(secondary),
            "narrow landing frame retained secondary metadata {secondary:?}"
        );
    }

    let short = rendered_application_rows_at(&application, 80, 6).join("\n");
    assert!(!short.contains("Chidori"));
    assert!(short.contains("What would you like to work on?"));
    assert!(short.contains("Keep the composer usable"));
    assert!(short.contains("Connected"));

    let too_small = rendered_application_rows_at(&application, 24, 4).join("\n");
    assert!(too_small.contains("Terminal too small"));
    assert!(!too_small.contains("Keep the composer usable"));
    assert!(!too_small.contains("Prompt"));
}

#[test]
fn session_shell_degrades_metadata_before_transcript_or_composer_content() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut active_snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    active_snapshot.session.status = SessionStatus::Active;
    active_snapshot.session.agent = Some(AgentIdentity {
        agent: AgentId::new("codex"),
        provider: ProviderId::new("openai"),
        model: ModelId::new("gpt-5"),
    });
    active_snapshot.turns[0].status = TurnStatus::Active;
    let activity_id = active_snapshot.activities[0].id();
    active_snapshot.activities[0] = Activity::Status {
        id: activity_id,
        turn_id: active_snapshot.turns[0].id,
        text: "Working".to_owned(),
    };

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(active_snapshot.clone()))
        .expect("attach active Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep the draft visible".to_owned(),
        )))
        .expect("type a Session draft");

    let wide = rendered_application_rows_at(&application, 100, 16).join("\n");
    for content in [
        "Chidori",
        "Workspace",
        workspace.path().to_string_lossy().as_ref(),
        "Connected",
        "Keep the transcript visible",
        "Keep the draft visible",
        "active",
        "Esc interrupt",
        "Agent codex",
        "Provider openai",
        "Model gpt-5",
        "Enter submit",
    ] {
        assert!(
            wide.contains(content),
            "wide Session frame omitted {content:?}"
        );
    }

    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    for core in [
        "Chidori",
        "Connected",
        "Keep the transcript visible",
        "Keep the draft visible",
        "active",
        "Esc interrupt",
    ] {
        assert!(
            narrow.contains(core),
            "narrow Session frame omitted {core:?}"
        );
    }
    for secondary in ["Workspace", "Provider", "Model", "Enter submit"] {
        assert!(
            !narrow.contains(secondary),
            "narrow Session frame retained secondary metadata {secondary:?}"
        );
    }

    let short = rendered_application_rows_at(&application, 100, 6).join("\n");
    assert!(!short.contains("Chidori"));
    assert!(!short.contains("Workspace"));
    assert!(short.contains("Working"));
    assert!(short.contains("Keep the draft visible"));
    assert!(short.contains("active"));
    assert!(short.contains("Connected"));

    let mut idle = Application::new(workspace.path());
    let mut idle_snapshot = active_snapshot;
    idle_snapshot.session.status = SessionStatus::Idle;
    idle_snapshot.session.agent = None;
    idle.handle_event(ApplicationEvent::SessionAttached(idle_snapshot))
        .expect("attach unavailable-Agent Session");
    let idle_frame = rendered_application_rows_at(&idle, 80, 12).join("\n");
    assert!(idle_frame.contains("idle"));
    assert!(idle_frame.contains("Agent unavailable"));
    assert!(!idle_frame.contains("Esc interrupt"));
    assert!(!idle_frame.contains("Provider"));
    assert!(!idle_frame.contains("Model"));

    let too_small = rendered_application_rows_at(&application, 24, 4).join("\n");
    assert!(too_small.contains("Terminal too small"));
    assert!(!too_small.contains("Keep the transcript visible"));
    assert!(!too_small.contains("Keep the draft visible"));
}

#[test]
fn command_activities_render_active_successful_and_failed_states_at_responsive_widths() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let cases = [
        (ActivityStatus::Active, None, "$ cargo test", Color::Cyan),
        (
            ActivityStatus::Completed,
            Some(0),
            "✓ cargo test",
            Color::Green,
        ),
        (
            ActivityStatus::Failed,
            Some(17),
            "× cargo test (exit 17)",
            Color::Red,
        ),
    ];

    for (status, exit_status, heading, color) in cases {
        let mut snapshot = failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Run the test suite",
            workspace.path(),
        );
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::Command {
            id: activity_id,
            turn_id: snapshot.turns[0].id,
            status,
            command: "cargo test".to_owned(),
            cwd: Some("/fixture/work".into()),
            output: "running tests\ntest result available\n".to_owned(),
            exit_status,
        };
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with command Activity");

        let desktop = rendered_application_buffer(&application, 100, 22);
        let desktop_text = buffer_rows(&desktop).join("\n");
        for expected in [
            heading,
            "in /fixture/work",
            "running tests",
            "test result available",
        ] {
            assert!(
                desktop_text.contains(expected),
                "desktop command Activity omitted {expected:?}:\n{desktop_text}"
            );
        }
        assert_eq!(text_cell(&desktop, heading).fg, color);

        let compact = rendered_application_rows_at(&application, 43, 18).join("\n");
        for expected in [
            heading,
            "in /fixture/work",
            "running tests",
            "test result available",
        ] {
            assert!(
                compact.contains(expected),
                "compact command Activity omitted {expected:?}:\n{compact}"
            );
        }
    }
}

#[test]
fn file_change_activities_render_active_successful_and_failed_states_at_responsive_widths() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let cases = [
        (
            ActivityStatus::Active,
            "… Applying file changes",
            Color::Cyan,
        ),
        (
            ActivityStatus::Completed,
            "✓ Applied file changes",
            Color::Green,
        ),
        (
            ActivityStatus::Failed,
            "× Failed to apply file changes",
            Color::Red,
        ),
    ];

    for (status, heading, color) in cases {
        let mut snapshot = failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Change these files",
            workspace.path(),
        );
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::FileChange {
            id: activity_id,
            turn_id: snapshot.turns[0].id,
            status,
            changes: vec![
                FileChange::Update {
                    path: "src/a.rs".into(),
                    moved_to: Some("src/b.rs".into()),
                },
                FileChange::Add {
                    path: "tests/new.rs".into(),
                },
                FileChange::Delete {
                    path: "old.rs".into(),
                },
            ],
        };
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with file-change Activity");

        let desktop = rendered_application_buffer(&application, 100, 22);
        let desktop_text = buffer_rows(&desktop).join("\n");
        for expected in [
            heading,
            "R src/a.rs → src/b.rs",
            "A tests/new.rs",
            "D old.rs",
        ] {
            assert!(
                desktop_text.contains(expected),
                "desktop file-change Activity omitted {expected:?}:\n{desktop_text}"
            );
        }
        assert_eq!(text_cell(&desktop, heading).fg, color);

        let compact = rendered_application_rows_at(&application, 43, 18).join("\n");
        for expected in [
            heading,
            "R src/a.rs → src/b.rs",
            "A tests/new.rs",
            "D old.rs",
        ] {
            assert!(
                compact.contains(expected),
                "compact file-change Activity omitted {expected:?}:\n{compact}"
            );
        }
    }
}

#[test]
fn streaming_command_updates_reuse_one_projected_transcript_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let mut snapshot = failed_session_snapshot(
        session_id,
        PromptId::new(),
        "Run the test suite",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let turn_id = snapshot.turns[0].id;
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Command {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Active,
        command: "cargo test".to_owned(),
        cwd: None,
        output: String::new(),
        exit_status: None,
    };
    let initial_revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach active command Activity");

    for (revision, content) in [
        (initial_revision.0 + 1, "running "),
        (initial_revision.0 + 2, "tests\n"),
    ] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                SessionUpdate {
                    session_id,
                    revision: SessionRevision(revision),
                    changes: vec![SessionChange::CommandOutputAppended {
                        activity_id,
                        content: content.to_owned(),
                    }],
                },
            )))
            .expect("project streamed command output");
    }
    let streamed = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert_eq!(streamed.matches("$ cargo test").count(), 1);
    assert_eq!(streamed.matches("running tests").count(), 1);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(initial_revision.0 + 3),
                changes: vec![SessionChange::CommandStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    exit_status: Some(0),
                }],
            },
        )))
        .expect("project command completion");
    let completed = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert_eq!(completed.matches("✓ cargo test").count(), 1);
    assert!(!completed.contains("$ cargo test"));
    assert_eq!(completed.matches("running tests").count(), 1);
}

#[test]
fn recovering_view_retains_the_landing_composer_and_last_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 2,
        retry_in: Duration::from_millis(500),
    }));

    let screen = rendered_state_rows(&state).join("\n");

    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Recovering"));
    assert!(screen.contains("pid 42424"));
}

#[test]
fn reconnect_overlay_waits_for_the_grace_period_and_blocks_composer_input() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let instance_id = fixture_instance_id();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Keep the Session visible",
            workspace.path(),
        )))
        .expect("attach Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Preserve this draft".to_owned(),
        )))
        .expect("type Session draft");

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: Duration::from_millis(50),
            },
        )))
        .expect("begin recovery");
    let brief = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(brief.contains("Recovering"));
    assert!(brief.contains("Keep the Session visible"));
    assert!(brief.contains("Preserve this draft"));
    assert!(!brief.contains("Reconnecting to Chidori"));
    assert!(!brief.contains("Your Session will resume automatically"));

    application
        .handle_event(ApplicationEvent::ReconnectGraceElapsed)
        .expect("show delayed reconnect overlay");
    let prolonged = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(prolonged.contains("Reconnecting to Chidori"));
    assert!(prolonged.contains("Your Session will resume automatically"));
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            " must stay blocked".to_owned(),
        )))
        .expect("reconnect mode owns composer input");

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("reconnect to surviving server");
    let recovered = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(!recovered.contains("Reconnecting to Chidori"));
    assert!(recovered.contains("Keep the Session visible"));
    assert!(recovered.contains("Preserve this draft"));
    assert!(!recovered.contains("must stay blocked"));
    assert!(recovered.contains("Connected"));
}

#[test]
fn recovered_view_switches_identity_on_the_confirmed_connection() {
    let previous_instance_id = Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002")
        .expect("parse previous instance ID");
    let recovered_instance_id = Uuid::parse_str("a4cc72ad-5507-4d4f-89f4-a3f7f1119d41")
        .expect("parse recovered instance ID");
    let mut state = connected_state(previous_instance_id, 42_424);
    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 1,
        retry_in: Duration::ZERO,
    }));
    state.apply(ManagedEvent::Connected(ready_health(
        recovered_instance_id,
        84_848,
    )));

    let recovered = rendered_state_rows(&state).join("\n");
    assert!(recovered.contains("Connected"));
    assert!(recovered.contains("pid 84848"));
    assert!(recovered.contains("a4cc72ad"));
}

#[test]
fn manual_stop_view_retains_the_landing_screen_and_last_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state.apply(ManagedEvent::ServerShutdown(ServerShutdown {
        instance_id,
        reason: ShutdownReason::Manual,
    }));

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Shared server stopped intentionally"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn fatal_protocol_error_is_rendered_visibly_with_the_last_known_state() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state.apply(ManagedEvent::Fatal(
        "server sent unknown event type 'future_event'".to_owned(),
    ));

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Connection failed"));
    assert!(screen.contains("unknown event type 'future_event'"));
}

#[test]
fn semantic_bindings_preserve_multiline_unicode_input_and_clear_before_exit() {
    let mut application = Application::default();
    let paste = command_for_terminal_event(InputEvent::Paste("a🙂β".to_owned()))
        .expect("map bracketed paste to editor input");
    application
        .handle_event(ApplicationEvent::Command(paste))
        .expect("paste Unicode text");
    application
        .handle_event(ApplicationEvent::Command(
            command_for_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .expect("map left cursor movement"),
        ))
        .expect("move over one Unicode character");
    application
        .handle_event(ApplicationEvent::Command(CommandId::DeleteBackward))
        .expect("delete the preceding Unicode character");

    for event in [
        InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        InputEvent::Paste("x\ny".to_owned()),
    ] {
        application
            .handle_event(ApplicationEvent::Command(
                command_for_terminal_event(event).expect("map multiline editor input"),
            ))
            .expect("edit multiline Prompt");
    }
    let screen = rendered_application_rows(&application).join("\n");
    assert!(screen.contains("a"));
    assert!(screen.contains("x"));
    assert!(screen.contains("yβ"));
    assert!(!screen.contains('🙂'));
    assert!(screen.contains("Enter submit"));
    assert!(screen.contains("Shift+Enter newline"));

    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))),
        Some(CommandId::SubmitSteer)
    );
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        ))),
        Some(CommandId::InsertNewline)
    );
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        ))),
        Some(CommandId::InsertNewline)
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("clear non-empty composer"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Type a Prompt")
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("exit with an empty composer"),
        ApplicationTransition::Exit
    );
}

#[test]
fn composer_grows_to_one_third_of_the_terminal_then_scrolls_internally() {
    let mut one_line = Application::default();
    one_line
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "one line".to_owned(),
        )))
        .expect("type short Prompt");
    let one_line_rows = rendered_application_rows_at(&one_line, 80, 30);
    assert_eq!(prompt_block_height(&one_line_rows), 3);

    let mut four_lines = Application::default();
    four_lines
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "one\ntwo\nthree\nfour".to_owned(),
        )))
        .expect("type multiline Prompt");
    let four_line_rows = rendered_application_rows_at(&four_lines, 80, 30);
    assert_eq!(prompt_block_height(&four_line_rows), 6);

    let mut long = Application::default();
    long.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        (1..=20)
            .map(|line| format!("line{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )))
    .expect("type long Prompt");
    let long_rows = rendered_application_rows_at(&long, 80, 30);
    let long_screen = long_rows.join("\n");
    assert_eq!(prompt_block_height(&long_rows), 12);
    assert!(long_screen.contains("line20"));
    assert!(!long_screen.contains("line01"));
}

#[test]
fn provisional_steer_is_immediate_single_and_reconciles_in_place() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, initial_snapshot) = enter_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Use the smaller interface".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt {
        session_id: admitted_to,
        request,
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    assert_eq!(admitted_to, session_id);
    let prompt_id = request.prompt.id;
    let provisional = rendered_application_rows(&application).join("\n");
    assert_eq!(provisional.matches("Use the smaller interface").count(), 1);
    assert!(provisional.contains("Type a Prompt"));

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("ignore overlapping submit key event"),
        ApplicationTransition::Continue
    );
    application
        .handle_event(ApplicationEvent::PromptAdmissionSucceeded(prompt_id))
        .expect("handle Prompt admission acknowledgement");
    assert_eq!(
        rendered_application_rows(&application)
            .join("\n")
            .matches("Use the smaller interface")
            .count(),
        1
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            delivered_update(
                session_id,
                SessionRevision(initial_snapshot.revision.0 + 1),
                prompt_id,
                &request.prompt.text,
            ),
        )))
        .expect("apply authoritative Prompt delivery");
    let reconciled = rendered_application_rows(&application).join("\n");
    assert_eq!(reconciled.matches("Use the smaller interface").count(), 1);
}

#[test]
fn admitted_active_steer_stays_visible_while_the_composer_accepts_another_prompt() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, snapshot, _) = enter_active_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep this pending steer visible".to_owned(),
        )))
        .expect("type active steer");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit active steer")
    else {
        panic!("active steer should request Prompt admission");
    };
    let prompt_id = request.prompt.id;
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: request.prompt.text,
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(3),
                        status: PromptStatus::Pending,
                    },
                }],
            },
        )))
        .expect("reconcile pending active steer");
    let pending = rendered_application_rows(&application).join("\n");
    assert_eq!(
        pending.matches("Keep this pending steer visible").count(),
        1
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "A second steer".to_owned(),
        )))
        .expect("type another steer while the first is pending");
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit another steer"),
        ApplicationTransition::AdmitPrompt { .. }
    ));
}

#[test]
fn queued_prompt_docks_immediately_and_scoped_mode_preserves_the_draft() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, snapshot, _) = enter_active_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Run this later".to_owned(),
        )))
        .expect("type queued Prompt");
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        ))),
        Some(CommandId::SubmitQueue)
    );
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitQueue))
        .expect("submit queued Prompt")
    else {
        panic!("Alt+Enter should request queued Prompt admission");
    };
    assert_eq!(request.delivery, PromptDelivery::Queue);
    let prompt_id = request.prompt.id;
    let optimistic = rendered_application_rows(&application).join("\n");
    assert!(optimistic.contains("Pending"));
    assert_eq!(optimistic.matches("Run this later").count(), 1);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: request.prompt.text,
                        delivery: PromptDelivery::Queue,
                        admission_order: PromptOrder(3),
                        status: PromptStatus::Pending,
                    },
                }],
            },
        )))
        .expect("apply authoritative queued Prompt");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "keep this draft".to_owned(),
        )))
        .expect("type a competing draft");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
            )))
            .expect("start command leader"),
        ApplicationTransition::Continue
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("open queued-Prompt mode");
    let ApplicationTransition::PromotePrompt {
        session_id: promoted_in,
        prompt_id: promoted,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("promote selected queued Prompt")
    else {
        panic!("Enter in queued-Prompt mode should promote the selection");
    };
    assert_eq!((promoted_in, promoted), (session_id, prompt_id));
    application
        .handle_event(ApplicationEvent::SessionOperationFailed(
            "competing mutation lost".to_owned(),
        ))
        .expect("report failed mutation");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("keep this draft")
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("restart command leader");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("reopen queued-Prompt mode");
    let ApplicationTransition::CancelPrompt {
        session_id: cancelled_in,
        prompt_id: cancelled,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )))
        .expect("cancel selected queued Prompt")
    else {
        panic!("Ctrl+D in queued-Prompt mode should cancel the selection");
    };
    assert_eq!((cancelled_in, cancelled), (session_id, prompt_id));
}

#[test]
fn escape_confirmation_is_local_and_targets_the_observed_active_turn() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (expected_session_id, snapshot, active_turn_id) =
        enter_active_session(&mut application, workspace.path());
    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a second local observer");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request interruption"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Esc again")
    );
    assert!(
        !rendered_application_rows(&observer)
            .join("\n")
            .contains("Esc again"),
        "interruption confirmation must remain client-local"
    );

    let ApplicationTransition::InterruptTurn {
        session_id,
        turn_id,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("confirm interruption")
    else {
        panic!("the second Esc should issue a targeted interruption");
    };
    assert_eq!(session_id, expected_session_id);
    assert_eq!(turn_id, active_turn_id);
}

#[test]
fn page_up_exposes_latest_and_end_resumes_following_the_transcript() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 8),
        ))
        .expect("attach a long Session");

    let latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(latest.contains("Agent section 8"));
    assert!(!latest.contains("Latest"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            )))
            .expect("page up through transcript content"),
        ApplicationTransition::Continue
    );
    let reading_history = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(reading_history.contains("Latest"));
    assert!(!reading_history.contains("Agent section 8"));

    for _ in 0..2 {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::PageDown,
                    KeyModifiers::NONE,
                )))
                .expect("page down through transcript content"),
            ApplicationTransition::Continue
        );
        rendered_application_rows_at(&application, 72, 18);
    }
    let paged_to_latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(paged_to_latest.contains("Agent section 8"));
    assert!(!paged_to_latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away again before using End");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Latest")
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .expect("return to latest transcript content"),
        ApplicationTransition::Continue
    );
    let resumed = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(resumed.contains("Agent section 8"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn a_scrolled_message_anchor_survives_streaming_and_terminal_resize_per_client() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let mut snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    snapshot.session.status = SessionStatus::Active;
    snapshot
        .turns
        .last_mut()
        .expect("fixture has a final Turn")
        .status = TurnStatus::Active;
    let streaming_message_id = snapshot
        .messages
        .last_mut()
        .map(|message| {
            message.status = MessageStatus::Streaming;
            message.id
        })
        .expect("fixture has a final Agent Message");

    let mut reader = Application::new(workspace.path());
    reader
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach historical reader");
    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach tail-following observer");
    rendered_application_rows_at(&reader, 72, 18);
    rendered_application_rows_at(&observer, 72, 18);
    reader
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("reader pages into history");
    let anchored_rows = rendered_application_rows_at(&reader, 72, 18);
    let anchor_row = rendered_row(&anchored_rows, "Agent section 5");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 5"),
        "expected the fifth Agent Message to be the visible anchor:\n{anchored}"
    );

    let appended = SessionUpdate {
        session_id,
        revision: SessionRevision(snapshot.revision.0 + 1),
        changes: vec![SessionChange::MessageContentAppended {
            message_id: streaming_message_id,
            content: "\n\nSTREAMED TAIL that only a client following the bottom should see"
                .to_owned(),
        }],
    };
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                appended.clone(),
            )))
            .expect("append streamed Agent content");
    }

    let resized_reader_rows = rendered_application_rows_at(&reader, 42, 15);
    assert_eq!(
        rendered_row(&resized_reader_rows, "Agent section 5"),
        anchor_row,
        "resize must keep the Message on the same rendered row"
    );
    let resized_reader = resized_reader_rows.join("\n");
    assert!(
        resized_reader.contains("Agent section 5"),
        "resize must preserve the logical Message anchor:\n{resized_reader}"
    );
    assert!(resized_reader.contains("Latest"));
    assert!(!resized_reader.contains("STREAMED TAIL"));
    let following_observer = rendered_application_rows_at(&observer, 42, 15).join("\n");
    assert!(following_observer.contains("STREAMED TAIL"));
    assert!(!following_observer.contains("Latest"));

    let completed = SessionUpdate {
        session_id,
        revision: SessionRevision(appended.revision.0 + 1),
        changes: vec![
            SessionChange::MessageCompleted {
                message_id: streaming_message_id,
            },
            SessionChange::TurnStatusChanged {
                turn_id: snapshot.turns.last().expect("fixture has a final Turn").id,
                status: TurnStatus::Completed,
            },
            SessionChange::SessionStatusChanged {
                status: SessionStatus::Idle,
            },
        ],
    };
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                completed.clone(),
            )))
            .expect("complete streamed Agent content");
    }
    let completed_reader_rows = rendered_application_rows_at(&reader, 56, 20);
    assert_eq!(
        rendered_row(&completed_reader_rows, "Agent section 5"),
        anchor_row,
        "completion must keep the Message on the same rendered row"
    );
    let completed_reader = completed_reader_rows.join("\n");
    assert!(completed_reader.contains("Agent section 5"));
    assert!(completed_reader.contains("Latest"));
    assert!(
        rendered_application_rows_at(&observer, 56, 20)
            .join("\n")
            .contains("STREAMED TAIL")
    );
}

#[test]
fn resize_that_reveals_the_whole_transcript_resumes_following() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 4),
        ))
        .expect("attach a long Session");
    rendered_application_rows_at(&application, 40, 12);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into transcript history");
    assert!(
        rendered_application_rows_at(&application, 40, 12)
            .join("\n")
            .contains("Latest")
    );

    let expanded = rendered_application_rows_at(&application, 100, 40).join("\n");
    assert!(expanded.contains("Agent section 4"));
    assert!(!expanded.contains("Latest"));

    let compact_again = rendered_application_rows_at(&application, 40, 12).join("\n");
    assert!(compact_again.contains("Agent section 4"));
    assert!(!compact_again.contains("Latest"));
}

#[test]
fn transcript_navigation_remains_correct_beyond_the_terminal_scroll_limit() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let prompt_id = PromptId::new();
    let content = format!("{}TAIL beyond u16", "x\n".repeat(65_700));
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            prompt_id,
            &content,
            workspace.path(),
        )))
        .expect("attach a transcript longer than Ratatui's local scroll offset");

    let latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(latest.contains("TAIL beyond u16"));
    assert!(!latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away from a very long tail");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Latest")
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )))
        .expect("return to the very long tail");
    let resumed = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(resumed.contains("TAIL beyond u16"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn transcript_navigation_reaches_tail_of_one_oversized_wrapped_line() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let terminal_width = 28;
    let transcript_width = terminal_width - 2;
    let agent_message = snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Agent)
        .expect("fixture has an Agent Message");
    agent_message.content = format!(
        "{} TAIL",
        "x".repeat(usize::from(transcript_width) * 65_700)
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach one Agent line longer than Ratatui's local scroll offset");

    let latest = rendered_application_rows_at(&application, terminal_width, 18).join("\n");
    assert!(latest.contains("TAIL"));
    assert!(!latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away from the oversized wrapped line tail");
    assert!(
        rendered_application_rows_at(&application, terminal_width, 18)
            .join("\n")
            .contains("Latest")
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )))
        .expect("return to the oversized wrapped line tail");
    let resumed = rendered_application_rows_at(&application, terminal_width, 18).join("\n");
    assert!(resumed.contains("TAIL"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn message_anchor_survives_prompt_reconciliation_and_composer_dock_layout_changes() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach a long Session");
    rendered_application_rows_at(&application, 72, 22);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into transcript history");
    let anchored_rows = rendered_application_rows_at(&application, 72, 22);
    let anchor_row = rendered_row(&anchored_rows, "Agent section 4");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 4"),
        "expected the fourth Agent Message to be the visible anchor:\n{anchored}"
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Reconciled line one\nline two\nline three\nline four\nline five\nfinal draft row"
                .to_owned(),
        )))
        .expect("grow the multiline composer");
    let growing_composer_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&growing_composer_rows, "Agent section 4"),
        anchor_row,
        "composer growth must keep the Message on the same rendered row"
    );
    let growing_composer = growing_composer_rows.join("\n");
    assert!(growing_composer.contains("Agent section 4"));
    assert!(growing_composer.contains("final draft row"));
    assert!(growing_composer.contains("Latest"));

    let queued_prompt_id = PromptId::new();
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: queued_prompt_id,
                        text: "Queued dock entry".to_owned(),
                        delivery: PromptDelivery::Queue,
                        admission_order: PromptOrder(9),
                        status: PromptStatus::Pending,
                    },
                }],
            },
        )))
        .expect("show a queued Prompt dock");
    let with_dock_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&with_dock_rows, "Agent section 4"),
        anchor_row,
        "queued dock changes must keep the Message on the same rendered row"
    );
    let with_dock = with_dock_rows.join("\n");
    assert!(with_dock.contains("Agent section 4"));
    assert!(with_dock.contains("Queued dock entry"));
    assert!(with_dock.contains("Latest"));

    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the multiline steer optimistically")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    let provisional_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&provisional_rows, "Agent section 4"),
        anchor_row,
        "provisional content must keep the Message on the same rendered row"
    );
    let provisional = provisional_rows.join("\n");
    assert!(provisional.contains("Agent section 4"));
    assert!(provisional.contains("Latest"));

    let delivered_turn_id = TurnId::new();
    let delivered_message_id = MessageId::new();
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 2),
                changes: vec![
                    SessionChange::PromptAdded {
                        prompt: Prompt {
                            id: request.prompt.id,
                            text: request.prompt.text.clone(),
                            delivery: PromptDelivery::Steer,
                            admission_order: PromptOrder(10),
                            status: PromptStatus::Delivered,
                        },
                    },
                    SessionChange::TurnAdded {
                        turn: Turn {
                            id: delivered_turn_id,
                            prompt_id: request.prompt.id,
                            status: TurnStatus::Active,
                        },
                    },
                    SessionChange::MessageAdded {
                        message: Message {
                            id: delivered_message_id,
                            turn_id: delivered_turn_id,
                            role: MessageRole::User,
                            status: MessageStatus::Completed,
                            content: request.prompt.text.clone(),
                        },
                    },
                ],
            },
        )))
        .expect("reconcile the optimistic Prompt to a stable Message");
    let reconciled_anchor_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&reconciled_anchor_rows, "Agent section 4"),
        anchor_row,
        "Message reconciliation must keep the anchor on the same rendered row"
    );
    let reconciled_anchor = reconciled_anchor_rows.join("\n");
    assert!(reconciled_anchor.contains("Agent section 4"));
    assert!(reconciled_anchor.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("start the queued-Prompt leader");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("open queued-Prompt mode");
    for code in [KeyCode::PageUp, KeyCode::PageDown] {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE,)))
                .expect("scoped mode owns transcript navigation keys"),
            ApplicationTransition::Continue
        );
    }
    let ApplicationTransition::PromotePrompt { prompt_id, .. } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("queued-Prompt selection remains active")
    else {
        panic!("Page keys must not escape queued-Prompt mode");
    };
    assert_eq!(prompt_id, queued_prompt_id);

    application
        .handle_event(ApplicationEvent::Command(CommandId::FollowLatest))
        .expect("return to the reconciled tail");
    let latest = rendered_application_rows_at(&application, 72, 22).join("\n");
    assert_eq!(latest.matches("Reconciled line one").count(), 1);
    assert!(!latest.contains("Latest"));
}

#[test]
fn ended_session_subscription_requests_a_fresh_snapshot_for_reconciliation() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, _) = enter_session(&mut application, workspace.path());

    assert_eq!(
        application
            .handle_event(ApplicationEvent::SessionSubscriptionEnded)
            .expect("handle ended Session subscription"),
        ApplicationTransition::SubscribeSession(session_id)
    );
}

#[test]
fn text_entered_while_the_first_session_is_created_becomes_its_draft() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit initial Prompt")
    else {
        panic!("landing submission should create a Session");
    };
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "next Prompt".to_owned(),
        )))
        .expect("begin the next Prompt while creation is pending");
    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("move the pending landing draft cursor");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            failed_session_snapshot(
                SessionId::new(),
                request.prompt.id,
                &request.prompt.text,
                workspace.path(),
            ),
        )))
        .expect("enter created Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "!".to_owned(),
        )))
        .expect("edit the migrated Session draft at its preserved cursor");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("next Promp!t")
    );
}

#[test]
fn failed_admission_restores_stable_prompt_and_saves_intervening_input_to_history() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    enter_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Do not lose this Prompt".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    let prompt_id = request.prompt.id;
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "newer local text".to_owned(),
        )))
        .expect("type while admission is pending");
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            prompt_id,
            error: "server unavailable".to_owned(),
        })
        .expect("roll back failed admission");
    let restored = rendered_application_rows(&application).join("\n");
    assert_eq!(restored.matches("Do not lose this Prompt").count(), 1);
    assert!(!restored.contains("newer local text"));
    assert!(!restored.contains("Use the smaller interface"));

    let ApplicationTransition::AdmitPrompt {
        request: exact_retry,
        ..
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("retry restored Prompt")
    else {
        panic!("restored Prompt should be retryable");
    };
    assert_eq!(exact_retry.prompt.id, prompt_id);
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            prompt_id,
            error: "still unavailable".to_owned(),
        })
        .expect("restore the exact retry");

    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("navigate to displaced local input");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("newer local text")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryNext))
        .expect("return to restored failed Prompt");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Do not lose this Prompt")
    );
}

#[test]
fn authoritative_delivery_after_an_ambiguous_failure_removes_the_restored_retry() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, snapshot) = enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Accepted despite transport failure".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            prompt_id: request.prompt.id,
            error: "response connection closed".to_owned(),
        })
        .expect("restore ambiguously failed Prompt");
    assert_eq!(
        rendered_application_rows(&application)
            .join("\n")
            .matches("Accepted despite transport failure")
            .count(),
        1
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            delivered_update(
                session_id,
                SessionRevision(snapshot.revision.0 + 1),
                request.prompt.id,
                &request.prompt.text,
            ),
        )))
        .expect("reconcile late authoritative delivery");
    let reconciled = rendered_application_rows(&application).join("\n");
    assert_eq!(
        reconciled
            .matches("Accepted despite transport failure")
            .count(),
        1
    );
    assert!(reconciled.contains("Type a Prompt"));
    assert!(!reconciled.contains("response connection closed"));
}

#[test]
fn multiline_history_is_boundary_aware_and_session_drafts_keep_their_cursor() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (first_session, first_snapshot) = enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "top\nbottom".to_owned(),
        )))
        .expect("type multiline draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("move within multiline draft before navigating history");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("bottom")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("navigate history at first-line boundary");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Initial Prompt")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryNext))
        .expect("restore multiline draft from history navigation");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("bottom")
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
        .expect("clear first Session draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "ac".to_owned(),
        )))
        .expect("type first Session draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("place first Session cursor between characters");

    let second_session = SessionId::new();
    let second_snapshot = failed_session_snapshot(
        second_session,
        PromptId::new(),
        "Second Session Prompt",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            second_snapshot,
        )))
        .expect("switch to second Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "second draft".to_owned(),
        )))
        .expect("type second Session draft");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            first_snapshot,
        )))
        .expect("switch back to first Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "b".to_owned(),
        )))
        .expect("insert at restored first Session cursor");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("abc")
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            failed_session_snapshot(
                second_session,
                PromptId::new(),
                "Second Session Prompt",
                workspace.path(),
            ),
        )))
        .expect("return to second Session");
    let second_screen = rendered_application_rows(&application).join("\n");
    assert!(second_screen.contains("second draft"));
    assert!(!second_screen.contains("abc"));
    assert_ne!(first_session, second_session);
}

fn prompt_block_height(rows: &[String]) -> usize {
    let top = rows
        .iter()
        .position(|row| row.contains('┌') && row.contains("Prompt"))
        .expect("Prompt block top border is rendered");
    let bottom = rows
        .iter()
        .enumerate()
        .skip(top + 1)
        .find_map(|(index, row)| row.contains('└').then_some(index))
        .expect("Prompt block bottom border is rendered");
    bottom - top + 1
}

fn enter_session(
    application: &mut Application,
    workspace: &std::path::Path,
) -> (SessionId, SessionSnapshot) {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit initial Prompt")
    else {
        panic!("landing submission should create a Session");
    };
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(
        session_id,
        request.prompt.id,
        &request.prompt.text,
        workspace,
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            snapshot.clone(),
        )))
        .expect("enter created Session");
    (session_id, snapshot)
}

fn enter_active_session(
    application: &mut Application,
    workspace: &std::path::Path,
) -> (SessionId, SessionSnapshot, TurnId) {
    let (session_id, mut snapshot) = enter_session(application, workspace);
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    snapshot.revision = SessionRevision(2);
    snapshot.session.status = SessionStatus::Active;
    snapshot.prompts.push(Prompt {
        id: prompt_id,
        text: "Long-running work".to_owned(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(2),
        status: PromptStatus::Delivered,
    });
    snapshot.turns.push(Turn {
        id: turn_id,
        prompt_id,
        status: TurnStatus::Active,
    });
    snapshot.messages.push(Message {
        id: message_id,
        turn_id,
        role: MessageRole::User,
        status: MessageStatus::Completed,
        content: "Long-running work".to_owned(),
    });
    snapshot
        .transcript
        .push(TranscriptItem::Message { message_id });
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach active Session");
    (session_id, snapshot, turn_id)
}

fn failed_session_snapshot(
    session_id: SessionId,
    prompt_id: PromptId,
    text: &str,
    workspace: &std::path::Path,
) -> SessionSnapshot {
    let delivered = FailedTurnFixture::new(prompt_id, text, PromptOrder::INITIAL);
    let transcript = delivered.transcript();
    SessionSnapshot {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent: None,
            status: SessionStatus::Idle,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![delivered.prompt],
        turns: vec![delivered.turn],
        messages: vec![delivered.message],
        activities: vec![delivered.activity],
        transcript,
    }
}

fn session_summary(
    session_id: SessionId,
    workspace: &std::path::Path,
    title: &str,
    status: SessionStatus,
    updated_at: u64,
) -> SessionSummary {
    SessionSummary {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent: None,
            status,
        },
        title: title.to_owned(),
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(updated_at),
    }
}

fn open_session_picker_with(application: &mut Application, sessions: Vec<SessionSummary>) {
    let workspace = sessions
        .first()
        .map(|summary| summary.session.workspace.path.clone())
        .unwrap_or_else(|| std::env::current_dir().expect("read current Workspace"));
    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                chidori::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate Session picker");
}

fn expect_session_list_request(
    transition: ApplicationTransition,
    expected_scope: SessionListScope,
) -> SessionListRequest {
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("expected Session listing transition");
    };
    assert_eq!(request.scope(), &expected_scope);
    request
}

fn navigable_session_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
    section_count: usize,
) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent: None,
            status: SessionStatus::Idle,
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: Vec::new(),
        messages: Vec::new(),
        activities: Vec::new(),
        transcript: Vec::new(),
    };
    for section in 1..=section_count {
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let user_message_id = MessageId::new();
        let agent_message_id = MessageId::new();
        snapshot.prompts.push(Prompt {
            id: prompt_id,
            text: format!("Prompt section {section}"),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(section as u64),
            status: PromptStatus::Delivered,
        });
        snapshot.turns.push(Turn {
            id: turn_id,
            prompt_id,
            status: TurnStatus::Completed,
        });
        snapshot.messages.extend([
            Message {
                id: user_message_id,
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: format!("Prompt section {section}"),
            },
            Message {
                id: agent_message_id,
                turn_id,
                role: MessageRole::Agent,
                status: MessageStatus::Completed,
                content: format!(
                    "## Agent section {section}\n\nA multiline Markdown response for section {section}."
                ),
            },
        ]);
        snapshot.transcript.extend([
            TranscriptItem::Message {
                message_id: user_message_id,
            },
            TranscriptItem::Message {
                message_id: agent_message_id,
            },
        ]);
    }
    snapshot
}

fn delivered_update(
    session_id: SessionId,
    revision: SessionRevision,
    prompt_id: PromptId,
    text: &str,
) -> SessionUpdate {
    let delivered = FailedTurnFixture::new(prompt_id, text, PromptOrder(revision.0));
    SessionUpdate {
        session_id,
        revision,
        changes: delivered.into_changes(),
    }
}

struct FailedTurnFixture {
    prompt: Prompt,
    turn: Turn,
    message: Message,
    activity: Activity,
}

impl FailedTurnFixture {
    fn new(prompt_id: PromptId, text: &str, admission_order: PromptOrder) -> Self {
        let turn_id = TurnId::new();
        Self {
            prompt: Prompt {
                id: prompt_id,
                text: text.to_owned(),
                delivery: PromptDelivery::Steer,
                admission_order,
                status: PromptStatus::Delivered,
            },
            turn: Turn {
                id: turn_id,
                prompt_id,
                status: TurnStatus::Failed,
            },
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: text.to_owned(),
            },
            activity: Activity::Error {
                id: ActivityId::new(),
                turn_id,
                text: "No Agent is selected".to_owned(),
            },
        }
    }

    fn transcript(&self) -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::Message {
                message_id: self.message.id,
            },
            TranscriptItem::Activity {
                activity_id: self.activity.id(),
            },
        ]
    }

    fn into_changes(self) -> Vec<SessionChange> {
        vec![
            SessionChange::PromptAdded {
                prompt: self.prompt,
            },
            SessionChange::TurnAdded { turn: self.turn },
            SessionChange::MessageAdded {
                message: self.message,
            },
            SessionChange::ActivityAdded {
                activity: self.activity,
            },
        ]
    }
}
