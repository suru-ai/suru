//! Composer input: cursor placement, growth, editing bindings, and history.

use crate::support::{
    buffer_rows, enter_session, failed_session_snapshot, rendered_application_buffer,
    rendered_application_cursor_at, rendered_application_rows, rendered_application_rows_at,
    text_position,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use suru::{
    managed_client::SessionEvent,
    protocol::{PromptId, SessionId},
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, command_for_terminal_event,
    },
};

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
