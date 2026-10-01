//! `/sidekick`: the semantic command that opens the Landing in the Sidekick
//! Workspace of the Server the Outlook is turned toward, which that Server
//! makes the first time it is asked for it. A Session begun there is a
//! Sidekick's; the command asks for nothing more than the Landing.

use crate::{
    connecting::turn_to_studio,
    support::{
        connected_application, enter_active_session, fixture_instance_id, ready_health,
        rendered_application_rows_at, type_terminal_text,
    },
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, Outlook, ResolvedWorkspace, SettingsSnapshot, SidebarVisibility,
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
