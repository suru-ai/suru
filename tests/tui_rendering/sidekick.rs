//! `/sidekick`: the semantic command that opens the Landing in the Sidekick
//! Workspace of the Server the Outlook is turned toward, which that Server
//! makes the first time it is asked for it. A Session begun there is a
//! Sidekick's; the command asks for nothing more than the Landing.
//!
//! A Message a Sidekick sent another Session on the user's behalf is drawn
//! apart from the user's own, as a Delegation is, naming the Sidekick, and
//! its heading is the way into the Sidekick's Session.

use crate::{
    connecting::turn_to_studio,
    support::{
        buffer_rows, click_mouse, connected_application, enter_active_session, fixture_instance_id,
        navigable_session_snapshot, ready_health, rendered_application_buffer,
        rendered_application_rows_at, text_position, type_terminal_text, workspace_dir,
    },
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        Author, EffectiveSettings, Message, MessageId, MessageRole, MessageStatus, Outlook, Prompt,
        PromptDelivery, PromptId, PromptOrder, PromptStatus, ResolvedWorkspace, SessionDeleted,
        SessionId, SessionReference, SessionSnapshot, SessionStatus, SessionTimestamp,
        SettingsSnapshot, SidebarVisibility, TranscriptItem, Turn, TurnId, TurnStatus,
        WorkspacePaths,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        WorkspaceResolutionSurface,
    },
};

/// A home of its own, holding the Workspace the client launched in and the
/// Server's Sidekick Workspace beside its data.
struct Home {
    _directory: tempfile::TempDir,
    workspace: std::path::PathBuf,
    sidekick: std::path::PathBuf,
}

fn home() -> Home {
    let directory = tempfile::tempdir().expect("create a home");
    let root = suru::paths::canonical(directory.path()).expect("read the home");
    let workspace = root.join("Projects").join("suru");
    let sidekick = root.join("data").join("sidekick");
    std::fs::create_dir_all(&workspace).expect("create the Workspace");
    std::fs::create_dir_all(&sidekick).expect("create the Sidekick Workspace");
    Home {
        _directory: directory,
        workspace,
        sidekick,
    }
}

/// How the Landing names a Workspace beneath its composer, home-relative as
/// the Server's own home makes it.
fn label(parts: &[&str]) -> String {
    std::iter::once("~")
        .chain(parts.iter().copied())
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR)
}

/// A client connected to its own Server, which reports `home` as its home,
/// with the Sidebar out of the way of the main view.
fn application_in(home: &Home) -> Application {
    let mut application = connected_application(&home.workspace);
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings,
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("deliver settings");
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42).with_workspace_paths(WorkspacePaths {
                home: Some(
                    home.workspace
                        .parent()
                        .and_then(std::path::Path::parent)
                        .expect("the Workspace lies beneath the home")
                        .to_str()
                        .expect("the home is UTF-8")
                        .into(),
                ),
                ..WorkspacePaths::default()
            }),
        )))
        .expect("connect");
    application
}

/// The line beneath the Landing's composer, which names where a new Session
/// begins.
fn landing_location(application: &Application) -> String {
    let screen = rendered_application_rows_at(application, 200, 30);
    let composer_bottom = screen
        .iter()
        .position(|row| row.contains('└'))
        .unwrap_or_else(|| panic!("the Landing draws its composer: {screen:#?}"));
    screen[composer_bottom + 1].trim().to_owned()
}

fn invoke_sidekick(application: &mut Application) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionSidekick,
        )))
        .expect("invoke /sidekick")
}

#[test]
fn the_sidekick_command_is_semantic_and_typed_as_a_slash_with_no_arguments() {
    let home = home();
    let mut application = application_in(&home);
    assert_eq!(
        SemanticCommandId::SessionSidekick.as_str(),
        "session.sidekick"
    );

    type_terminal_text(&mut application, "/sidek");
    let offered = rendered_application_rows_at(&application, 200, 30).join("\n");
    assert!(
        offered.contains("/sidekick"),
        "the slash is offered as it is typed: {offered}"
    );
    let transition = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("choose /sidekick");
    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id: _,
    } = transition
    else {
        panic!("/sidekick asks the Outlook's Server for its Sidekick Workspace: {transition:?}");
    };
    assert_eq!(outlook, Outlook::Local);
    assert_eq!(
        landing_location(&application),
        label(&["Projects", "suru"]),
        "nothing moves until the Server answers"
    );
}

#[test]
fn the_landing_opens_in_the_sidekick_workspace_once_the_server_answers_from_an_open_session() {
    let home = home();
    let mut application = application_in(&home);
    enter_active_session(&mut application, &home.workspace);
    type_terminal_text(&mut application, "half a thought");

    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id,
    } = invoke_sidekick(&mut application)
    else {
        panic!("/sidekick asks for the Sidekick Workspace");
    };
    let transition = application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface: WorkspaceResolutionSurface::Sidekick,
            request_id,
            result: Ok(ResolvedWorkspace::directory(home.sidekick.clone())),
        })
        .expect("deliver the Sidekick Workspace");
    assert_eq!(
        transition,
        ApplicationTransition::DetachSession,
        "the Session being left is let go of, and the Landing opens"
    );
    assert_eq!(
        landing_location(&application),
        label(&["data", "sidekick"]),
        "the Landing stands in the Sidekick Workspace, so a Session begun there is a Sidekick's"
    );
    let screen = rendered_application_rows_at(&application, 200, 30).join("\n");
    assert!(
        !screen.contains("half a thought"),
        "a fresh Landing carries no draft over: {screen}"
    );
}

#[test]
fn turned_toward_a_remote_the_sidekick_command_asks_that_remote() {
    let mut application = Application::default();
    turn_to_studio(&mut application);

    let transition = invoke_sidekick(&mut application);
    assert!(
        matches!(
            &transition,
            ApplicationTransition::ResolveSidekickWorkspace { outlook, .. }
                if outlook == &Outlook::Remote("studio".to_owned())
        ),
        "a Remote's own Sidekick is reached the way everything else on it is: {transition:?}"
    );
}

#[test]
fn a_refused_sidekick_workspace_leaves_the_reader_where_they_were_saying_why() {
    let home = home();
    let mut application = application_in(&home);
    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id,
    } = invoke_sidekick(&mut application)
    else {
        panic!("/sidekick asks for the Sidekick Workspace");
    };
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface: WorkspaceResolutionSurface::Sidekick,
            request_id,
            result: Err("the disk is full".to_owned()),
        })
        .expect("deliver the refusal");

    let screen = rendered_application_rows_at(&application, 200, 30).join("\n");
    assert!(
        screen.contains("Could not open the Sidekick Workspace: the disk is full"),
        "{screen}"
    );
    assert_eq!(landing_location(&application), label(&["Projects", "suru"]));
}

#[test]
fn an_answer_to_a_superseded_sidekick_command_is_ignored() {
    let home = home();
    let mut application = application_in(&home);
    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id: first,
    } = invoke_sidekick(&mut application)
    else {
        panic!("/sidekick asks for the Sidekick Workspace");
    };
    let ApplicationTransition::ResolveSidekickWorkspace { .. } = invoke_sidekick(&mut application)
    else {
        panic!("asking again asks again");
    };
    assert_eq!(
        application
            .handle_event(ApplicationEvent::WorkspaceResolved {
                outlook,
                surface: WorkspaceResolutionSurface::Sidekick,
                request_id: first,
                result: Ok(ResolvedWorkspace::directory(home.sidekick.clone())),
            })
            .expect("deliver the stale answer"),
        ApplicationTransition::Continue
    );
    assert_eq!(landing_location(&application), label(&["Projects", "suru"]));
}

/// Answers a `/sidekick` the reader asked for earlier with the Sidekick
/// Workspace, as a slow Server would once they had moved on.
fn answer_late(
    application: &mut Application,
    home: &Home,
    outlook: Outlook,
    request_id: u64,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface: WorkspaceResolutionSurface::Sidekick,
            request_id,
            result: Ok(ResolvedWorkspace::directory(home.sidekick.clone())),
        })
        .expect("deliver the late answer")
}

#[test]
fn a_late_answer_after_the_reader_went_to_a_fresh_landing_leaves_their_new_draft_alone() {
    let home = home();
    let mut application = application_in(&home);
    enter_active_session(&mut application, &home.workspace);
    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id,
    } = invoke_sidekick(&mut application)
    else {
        panic!("/sidekick asks for the Sidekick Workspace");
    };
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("go to a fresh Landing instead");
    type_terminal_text(&mut application, "a different thought");

    assert_eq!(
        answer_late(&mut application, &home, outlook, request_id),
        ApplicationTransition::Continue,
        "the reader went elsewhere, so the answer has nothing left to answer"
    );
    assert_eq!(landing_location(&application), label(&["Projects", "suru"]));
    let screen = rendered_application_rows_at(&application, 200, 30).join("\n");
    assert!(screen.contains("a different thought"), "{screen}");
}

#[test]
fn a_late_answer_after_the_reader_began_a_session_leaves_it_open_with_its_draft() {
    let home = home();
    let mut application = application_in(&home);
    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id,
    } = invoke_sidekick(&mut application)
    else {
        panic!("/sidekick asks for the Sidekick Workspace");
    };
    enter_active_session(&mut application, &home.workspace);
    type_terminal_text(&mut application, "half a reply");

    assert_eq!(
        answer_late(&mut application, &home, outlook, request_id),
        ApplicationTransition::Continue,
        "the Session the reader began is not detached"
    );
    let screen = rendered_application_rows_at(&application, 200, 30).join("\n");
    assert!(screen.contains("half a reply"), "{screen}");
    assert!(
        !screen.contains(&label(&["data", "sidekick"])),
        "the Landing did not open in the Sidekick Workspace: {screen}"
    );
}

/// A Session the user began, whose second Turn a Prompt the Sidekick of
/// `sidekick`, titled `title`, sent on the user's behalf opened.
fn prompted_by_a_sidekick(
    workspace: &std::path::Path,
    sidekick: SessionId,
    title: &str,
) -> SessionSnapshot {
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace, 1);
    let author = Some(Author::Sidekick {
        session_id: sidekick,
        title: title.to_owned(),
    });
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    snapshot.prompts.push(Prompt {
        id: prompt_id,
        text: "Cover the empty input as well.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(2),
        status: PromptStatus::Delivered,
        author: author.clone(),
        withdrawal: None,
    });
    snapshot.turns.push(Turn {
        id: turn_id,
        prompt_id: Some(prompt_id),
        status: TurnStatus::Completed,
        ..snapshot.turns[0].clone()
    });
    snapshot.messages.push(Message {
        id: message_id,
        turn_id,
        role: MessageRole::User,
        status: MessageStatus::Completed,
        content: "Cover the empty input as well.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        truncated: false,
        author,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Message { message_id });
    snapshot
}

/// Presses the primary pointer button on the first rendered occurrence of
/// `needle`, the way a reader clicks what they can see.
fn press_text(
    application: &mut Application,
    width: u16,
    height: u16,
    needle: &str,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, width, height);
    let (column, row) = text_position(&buffer, needle);
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press what is drawn")
}

#[test]
fn a_message_a_sidekick_sent_is_drawn_apart_from_the_users_naming_the_sidekick() {
    let workspace = workspace_dir();
    let snapshot = prompted_by_a_sidekick(workspace.path(), SessionId::new(), "Tidy the listing");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Session a Sidekick prompted");
    let buffer = rendered_application_buffer(&application, 80, 22);
    let text = buffer_rows(&buffer).join("\n");

    assert!(
        text.contains("┃ Prompt section 1"),
        "the user's own Message wears the user's bar: {text}"
    );
    assert!(
        text.contains("│ Sent by Sidekick · Tidy the listing"),
        "a Sidekick's Message names the Sidekick that sent it: {text}"
    );
    assert!(
        text.contains("│ Cover the empty input as well."),
        "and says what it asked, down a bar of its own: {text}"
    );
    assert!(
        !text.contains("┃ Cover the empty input") && !text.contains("┃ Sent by"),
        "a Sidekick's Message never wears the user's bar: {text}"
    );
    let (user_bar, user_row) = text_position(&buffer, "┃ Prompt section 1");
    let (sidekick_bar, sidekick_row) = text_position(&buffer, "│ Cover the empty input");
    assert_ne!(
        buffer[(sidekick_bar, sidekick_row)].fg,
        buffer[(user_bar, user_row)].fg,
        "a Sidekick's bar is drawn in another role than the user's"
    );
    assert_eq!(
        buffer[(sidekick_bar + 2, sidekick_row)].bg,
        buffer[(user_bar + 2, user_row)].bg,
        "though both sit on the surface of what an Agent was asked"
    );
}

#[test]
fn a_sidekick_known_by_no_title_is_still_named_a_sidekick() {
    let workspace = workspace_dir();
    let snapshot = prompted_by_a_sidekick(workspace.path(), SessionId::new(), "");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Session a Sidekick prompted");
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");

    assert!(text.contains("│ Sent by a Sidekick"), "{text}");
    assert!(!text.contains("Sent by Sidekick ·"), "{text}");
}

#[test]
fn pressing_the_sidekicks_name_opens_the_sidekicks_session() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let snapshot = prompted_by_a_sidekick(workspace.path(), sidekick, "Tidy the listing");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Session a Sidekick prompted");

    assert_eq!(
        press_text(&mut application, 80, 22, "Cover the empty input"),
        ApplicationTransition::Continue,
        "what the Sidekick asked is text to select, not a way anywhere"
    );
    assert_eq!(
        press_text(&mut application, 80, 22, "Sent by Sidekick"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick,
        )),
        "the Sidekick's name leads into its Session"
    );
}

#[test]
fn opening_a_sidekick_is_a_semantic_command_a_message_invokes() {
    assert_eq!(SemanticCommandId::SidekickOpen.as_str(), "sidekick.open");
}

/// A Session the user began and settled, to which the Sidekick of
/// `sidekick` has since sent a Prompt that waits, undelivered, while the
/// Session's Provider starts: Working for it, with no Turn yet to stand in.
fn awaiting_a_sidekicks_prompt(
    workspace: &std::path::Path,
    sidekick: SessionId,
) -> SessionSnapshot {
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace, 1);
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp(1_755_000_000_000));
    snapshot.prompts.push(Prompt {
        id: PromptId::new(),
        text: "Cover the empty input as well.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(2),
        status: PromptStatus::Pending,
        author: Some(Author::Sidekick {
            session_id: sidekick,
            title: "Tidy the listing".to_owned(),
        }),
        withdrawal: None,
    });
    snapshot
}

/// A Session working on the user's Turn, behind which the Sidekick of
/// `sidekick` has queued a Prompt, and the user one of their own.
fn queued_behind_a_working_turn(
    workspace: &std::path::Path,
    sidekick: SessionId,
) -> SessionSnapshot {
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace, 1);
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp(1_755_000_000_000));
    snapshot.turns[0].status = TurnStatus::Active;
    for (text, order, author) in [
        (
            "Then write the changelog entry.",
            2,
            Some(Author::Sidekick {
                session_id: sidekick,
                title: "Tidy the listing".to_owned(),
            }),
        ),
        ("And tell me when it is done.", 3, None),
    ] {
        snapshot.prompts.push(Prompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Queue,
            admission_order: PromptOrder(order),
            status: PromptStatus::Pending,
            author,
            withdrawal: None,
        });
    }
    snapshot
}

/// The client hears the Session `session_id` was deleted, as its catalog
/// stream says so.
fn hear_deleted(application: &mut Application, session_id: SessionId) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted { session_id },
        )))
        .expect("hear the Session was deleted");
}

#[test]
fn a_prompt_a_sidekick_sent_is_drawn_apart_while_the_providers_startup_is_held() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            awaiting_a_sidekicks_prompt(workspace.path(), sidekick),
        ))
        .expect("attach the Session waiting on its Provider");
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");

    assert!(
        text.contains("│ Sent by Sidekick · Tidy the listing")
            && text.contains("│ Cover the empty input as well."),
        "before it is delivered, the Prompt stands as the Sidekick's Message will: {text}"
    );
    assert!(
        !text.contains("┃ Cover the empty input"),
        "and never as the user's own words: {text}"
    );
    assert_eq!(
        press_text(&mut application, 80, 22, "Sent by Sidekick"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick,
        )),
        "its heading already leads into the Sidekick's Session"
    );
}

#[test]
fn a_queued_prompt_a_sidekick_sent_names_the_sidekick_among_the_pending() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            queued_behind_a_working_turn(workspace.path(), sidekick),
        ))
        .expect("attach the working Session");
    let text = rendered_application_rows_at(&application, 100, 24).join("\n");

    assert!(
        text.contains("Sidekick · Tidy the listing: Then write the changelog entry."),
        "a queued Prompt names the Sidekick that sent it: {text}"
    );
    assert!(
        text.contains("  And tell me when it is done."),
        "the user's own stays as it was: {text}"
    );
    assert_eq!(
        press_text(&mut application, 100, 24, "Sidekick · Tidy the listing"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick,
        )),
        "the name leads into the Sidekick's Session"
    );
}

#[test]
fn a_sidekick_whose_session_is_gone_is_still_named_but_offers_no_way_in() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(prompted_by_a_sidekick(
            workspace.path(),
            sidekick,
            "Tidy the listing",
        )))
        .expect("attach the Session a Sidekick prompted");
    hear_deleted(&mut application, sidekick);

    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("│ Sent by Sidekick · Tidy the listing"),
        "the Message still names the Sidekick by the Title it sent under: {text}"
    );
    assert_eq!(
        press_text(&mut application, 80, 22, "Sent by Sidekick"),
        ApplicationTransition::Continue,
        "but leads nowhere, since there is no Session left to open"
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SidekickOpen,
            )))
            .expect("invoke the command naming no Session"),
        ApplicationTransition::Continue,
    );

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            queued_behind_a_working_turn(workspace.path(), sidekick),
        ))
        .expect("attach the working Session");
    hear_deleted(&mut application, sidekick);
    assert!(
        rendered_application_rows_at(&application, 100, 24)
            .join("\n")
            .contains("Sidekick · Tidy the listing: Then write the changelog entry."),
        "a queued Prompt still names its gone Sidekick"
    );
    assert_eq!(
        press_text(&mut application, 100, 24, "Sidekick · Tidy the listing"),
        ApplicationTransition::Continue,
        "but leads nowhere either"
    );
}

#[test]
fn a_sidekicks_session_that_cannot_be_opened_leaves_the_reader_where_they_were_saying_why() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(prompted_by_a_sidekick(
            workspace.path(),
            sidekick,
            "Tidy the listing",
        )))
        .expect("attach the Session a Sidekick prompted");

    let reference = SessionReference::new(Outlook::Local, sidekick);
    assert_eq!(
        press_text(&mut application, 80, 22, "Sent by Sidekick"),
        ApplicationTransition::ViewAndAttachSession(reference.clone()),
    );
    assert!(
        rendered_application_rows_at(&application, 80, 22)
            .join("\n")
            .contains("│ Cover the empty input as well."),
        "the Transcript stays on show until the Sidekick's Session is in hand"
    );

    application
        .handle_event(ApplicationEvent::OriginSessionAttachFailed {
            reference,
            error: "The Session does not exist on this Suru server.".to_owned(),
        })
        .expect("deliver the failed attach");
    let text = rendered_application_rows_at(&application, 160, 22).join("\n");
    assert!(
        text.contains("│ Cover the empty input as well."),
        "the reader is left on the Transcript they were reading: {text}"
    );
    assert!(
        text.contains("Could not open the Sidekick's Session: The Session does not exist"),
        "told why the Sidekick's Session did not open: {text}"
    );
}

#[test]
fn a_sidekick_title_too_long_for_its_row_is_cut_short_and_still_leads_in() {
    let workspace = workspace_dir();
    let sidekick = SessionId::new();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(prompted_by_a_sidekick(
            workspace.path(),
            sidekick,
            "Tidy the listing, then go through every Workspace and settle what is done",
        )))
        .expect("attach the Session a Sidekick prompted");
    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("│ Sent by Sidekick · Tidy the listing"),
        "the heading keeps to one row: {text}"
    );
    assert!(
        !text.contains("settle what is done"),
        "cut short where it runs long, rather than wrapped: {text}"
    );
    assert_eq!(
        press_text(&mut application, 80, 22, "Sent by Sidekick"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick,
        )),
    );
}
