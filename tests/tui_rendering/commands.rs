//! Slash command dispatch and composer autocomplete.

use crate::support::{
    buffer_rows, enter_active_session, failed_session_snapshot, rendered_application_buffer,
    rendered_application_cursor_at, rendered_application_rows, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use suru::{
    protocol::{PromptId, SessionId},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, CompletionMode},
};

#[test]
fn slash_autocomplete_invokes_new_session_from_a_description_match() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
    assert!(landing.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(landing.contains("Type a Prompt and press Enter"));
    assert!(!landing.contains("Long-running work"));
    assert!(!landing.contains("/new"));
}

#[test]
fn slash_settle_sets_the_open_session_aside() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, _, _) = enter_active_session(&mut application, workspace.path());

    type_terminal_text(&mut application, "/settle");

    let autocomplete = rendered_application_rows(&application).join("\n");
    assert!(autocomplete.contains("/settle"));
    assert!(autocomplete.contains("done for now"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke the settle command"),
        ApplicationTransition::SettleSession {
            session: suru::protocol::SessionReference::new(
                suru::protocol::Outlook::Local,
                session_id,
            ),
            settled: true,
        }
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("/settle"),
        "invoking the command closes the autocomplete it was chosen from"
    );
}

/// On the Landing there is no Session to set aside, so the command names none
/// and the view stays where it is.
#[test]
fn slash_settle_on_the_landing_sets_nothing_aside() {
    let mut application = Application::default();

    type_terminal_text(&mut application, "/settle");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke the settle command with no Session open"),
        ApplicationTransition::Continue
    );
}

#[test]
fn insert_completion_replaces_its_range_and_keeps_the_composer_open() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "Ask $revlater");

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ActivateCompletion(
                CompletionMode::insertion("rev", 4..8, "$review"),
            ),))
            .expect("activate inert insert completion"),
        ApplicationTransition::Continue
    );

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("confirm inert insert completion"),
        ApplicationTransition::Continue
    );

    type_terminal_text(&mut application, "now ");
    let ApplicationTransition::CreateSession(request) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit the completed Prompt separately")
    else {
        panic!("completion confirmation should not submit the composer");
    };
    assert_eq!(request.prompt.text, "Ask $review now later");
}

#[test]
fn exit_and_quit_commands_dispatch_one_semantic_exit_action() {
    let mut canonical = Application::default();
    type_terminal_text(&mut canonical, "/exit");
    let autocomplete = rendered_application_rows(&canonical).join("\n");
    assert!(autocomplete.contains("/exit"));
    assert!(autocomplete.contains("Exit Suru"));
    assert_eq!(
        canonical
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /exit"),
        ApplicationTransition::Exit
    );

    let mut alias = Application::default();
    type_terminal_text(&mut alias, "/quit");
    assert!(
        rendered_application_rows(&alias)
            .join("\n")
            .contains("/exit")
    );
    assert_eq!(
        alias
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /quit"),
        ApplicationTransition::Exit
    );

    assert_eq!(
        suru::tui::SemanticCommandId::ApplicationExit.as_str(),
        "application.exit"
    );
}

#[test]
fn models_commands_dispatch_one_semantic_model_list_action() {
    let mut canonical = Application::default();
    type_terminal_text(&mut canonical, "/models");
    assert!(
        rendered_application_rows(&canonical)
            .join("\n")
            .contains("Choose Model")
    );
    assert!(matches!(
        canonical
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /models"),
        ApplicationTransition::ListModels(_)
    ));

    let mut alias = Application::default();
    type_terminal_text(&mut alias, "/mo");
    assert!(
        rendered_application_rows(&alias)
            .join("\n")
            .contains("/models")
    );
    assert!(matches!(
        alias
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /mo"),
        ApplicationTransition::ListModels(_)
    ));

    let mut keybinding = Application::default();
    keybinding
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("begin semantic leader keybinding");
    assert!(matches!(
        keybinding
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::NONE,
            )))
            .expect("invoke Model picker"),
        ApplicationTransition::ListModels(_)
    ));
}

#[test]
fn themes_command_alias_and_keybinding_open_the_same_picker() {
    let mut canonical = Application::default();
    type_terminal_text(&mut canonical, "/themes");
    assert!(
        rendered_application_rows(&canonical)
            .join("\n")
            .contains("Choose Theme")
    );
    assert_eq!(
        canonical
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /themes"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&canonical)
            .join("\n")
            .contains(" Themes ")
    );

    let mut alias = Application::default();
    type_terminal_text(&mut alias, "/theme");
    assert!(
        rendered_application_rows(&alias)
            .join("\n")
            .contains("/themes")
    );
    assert_eq!(
        alias
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /theme"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&alias)
            .join("\n")
            .contains(" Themes ")
    );

    let mut keybinding = Application::default();
    keybinding
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("begin semantic leader keybinding");
    assert_eq!(
        keybinding
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::NONE,
            )))
            .expect("invoke Theme picker"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&keybinding)
            .join("\n")
            .contains(" Themes ")
    );

    assert_eq!(
        suru::tui::SemanticCommandId::ThemeList.as_str(),
        "theme.list"
    );
}

#[test]
fn options_commands_dispatch_one_semantic_model_options_action() {
    let mut canonical = Application::default();
    type_terminal_text(&mut canonical, "/options");
    assert!(
        rendered_application_rows(&canonical)
            .join("\n")
            .contains("Configure Model Options")
    );
    assert!(matches!(
        canonical
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /options"),
        ApplicationTransition::ListModels(_)
    ));

    let mut alias = Application::default();
    type_terminal_text(&mut alias, "/variants");
    assert!(
        rendered_application_rows(&alias)
            .join("\n")
            .contains("/options")
    );
    assert!(matches!(
        alias
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /variants"),
        ApplicationTransition::ListModels(_)
    ));

    let mut keybinding = Application::default();
    keybinding
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("begin semantic leader keybinding");
    assert!(matches!(
        keybinding
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::NONE,
            )))
            .expect("invoke Model Options"),
        ApplicationTransition::ListModels(_)
    ));

    assert_eq!(
        suru::tui::SemanticCommandId::ModelOptions.as_str(),
        "model.options"
    );
    assert_eq!(
        suru::tui::SemanticCommandId::ModelOptionsApply.as_str(),
        "model.options.apply"
    );
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
fn dismissed_alias_is_submitted_literally_before_turn_interruption() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
fn autocomplete_tracks_the_active_composer_when_a_session_attaches() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
