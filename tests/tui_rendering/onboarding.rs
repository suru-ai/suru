//! Onboarding: the three-step pass a Client offers over the Landing of its
//! own Server until that Server has been onboarded, and `/onboarding` takes
//! again whenever the reader likes.

use std::{collections::HashSet, path::Path};

use crate::support::{
    connected_application, deliver_settings, enter_session, fixture_instance_id, ready_health,
    rendered_application_rows_at, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, Health, Outlook, RemoteHealth, RemoteStatus, SettingsDiagnostic,
        SettingsDiagnosticSeverity, SettingsSnapshot, SidebarSettings, SidebarVisibility, Way,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        TerminalFacts,
    },
};

/// What a Server that has never been onboarded says of itself.
fn unonboarded_health() -> Health {
    ready_health(fixture_instance_id(), 42_424).with_onboarded(false)
}

fn connect(application: &mut Application, health: Health) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(health)))
        .expect("connect to the local Server");
}

/// A Client attached to its own Server, which has never been onboarded and
/// has not yet sent the effective-settings snapshot.
fn attached_to_a_fresh_server(workspace: &Path) -> Application {
    let mut application = Application::new(workspace, TerminalFacts::default());
    connect(&mut application, unonboarded_health());
    application
}

/// A fresh Server's Client once it holds the settings snapshot too, which is
/// the moment it offers Onboarding.
fn offered_onboarding(workspace: &Path) -> Application {
    let mut application = attached_to_a_fresh_server(workspace);
    deliver_settings(&mut application, EffectiveSettings::default());
    application
}

fn screen(application: &Application) -> String {
    rendered_application_rows_at(application, 100, 24).join("\n")
}

fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    press_with(application, code, KeyModifiers::NONE)
}

fn press_with(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("handle a key")
}

fn onboarding_shown(application: &Application) -> bool {
    screen(application).contains(" of 3")
}

#[test]
fn a_client_offers_onboarding_over_the_landing_once_it_holds_health_and_settings() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    assert!(
        !onboarding_shown(&application),
        "health alone is not enough: {}",
        screen(&application)
    );

    deliver_settings(&mut application, EffectiveSettings::default());
    let offered = screen(&application);
    assert!(offered.contains("Step 1 of 3"), "{offered}");
    assert!(offered.contains("Providers"), "{offered}");
    assert!(
        offered.contains("Agent unavailable"),
        "over the Landing, whose status still shows beneath: {offered}"
    );

    let mut settings_first = Application::new(workspace.path(), TerminalFacts::default());
    deliver_settings(&mut settings_first, EffectiveSettings::default());
    assert!(!onboarding_shown(&settings_first), "nor are settings alone");
    connect(&mut settings_first, unonboarded_health());
    assert!(screen(&settings_first).contains("Step 1 of 3"));
}

#[test]
fn a_client_of_an_onboarded_server_offers_nothing() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

/// Opens `/connect`, as a reader does, listing the Local row first and the
/// Remote `studio` beneath it.
fn open_connect(application: &mut Application) {
    type_terminal_text(application, "/connect");
    press(application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            suru::protocol::Remote {
                name: "studio".to_owned(),
                fingerprint: "studio-fingerprint".to_owned(),
                ways: vec![Way::Direct("10.0.0.8:7777".parse().unwrap())],
                status: RemoteStatus::Available,
            },
        ]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(suru::protocol::PROTOCOL_VERSION),
                status: RemoteStatus::Available,
                unreachable: None,
            }),
        })
        .unwrap();
}

/// Turns the Outlook through `/connect` to the row `key` moves to from the
/// Outlook on screen.
fn turn_outlook(application: &mut Application, key: KeyCode) -> ApplicationTransition {
    open_connect(application);
    press(application, key);
    press(application, KeyCode::Enter)
}

#[test]
fn onboarding_offered_beneath_connect_is_withdrawn_once_a_remote_is_chosen() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    open_connect(&mut application);
    deliver_settings(&mut application, EffectiveSettings::default());

    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote("studio".to_owned()),
            catalog_origins: HashSet::from([Outlook::Remote("studio".to_owned())]),
        }
    );
    assert!(
        !onboarding_shown(&application),
        "what was offered beneath Connect is not left over the Remote: {}",
        screen(&application)
    );

    turn_outlook(&mut application, KeyCode::Up);
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "back on its own Server, the Client offers it again: {}",
        screen(&application)
    );
}

#[test]
fn a_client_viewing_a_remote_offers_no_onboarding_there() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    assert_eq!(
        turn_outlook(&mut application, KeyCode::Down),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote("studio".to_owned()),
            catalog_origins: HashSet::from([Outlook::Remote("studio".to_owned())]),
        }
    );

    deliver_settings(&mut application, EffectiveSettings::default());
    assert!(
        !onboarding_shown(&application),
        "the Remote's Onboarding is its own Server's affair: {}",
        screen(&application)
    );
    connect(&mut application, unonboarded_health());
    assert!(
        !onboarding_shown(&application),
        "nor does a reconnect bring it over the Remote: {}",
        screen(&application)
    );

    assert_eq!(
        turn_outlook(&mut application, KeyCode::Up),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            catalog_origins: HashSet::new(),
        }
    );
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "back on its own Server, the Client offers it: {}",
        screen(&application)
    );
}

#[test]
fn the_header_counts_the_steps_as_next_and_back_move_between_them() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let first = screen(&application);
    assert!(first.contains("Step 1 of 3"), "{first}");
    assert!(first.contains("Next"), "{first}");
    assert!(!first.contains("Back"), "the first step has none: {first}");
    assert_eq!(
        press(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert!(screen(&application).contains("Step 1 of 3"));

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    let second = screen(&application);
    assert!(second.contains("Step 2 of 3"), "{second}");
    assert!(second.contains("Derivation"), "{second}");
    assert!(second.contains("Back"), "{second}");

    press(&mut application, KeyCode::Left);
    assert!(screen(&application).contains("Step 1 of 3"));
    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Enter);
    let third = screen(&application);
    assert!(third.contains("Step 3 of 3"), "{third}");
    assert!(third.contains("Icons"), "{third}");
    assert!(third.contains("Finish"), "the last step finishes: {third}");
    press_with(&mut application, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(screen(&application).contains("Step 2 of 3"));
}

#[test]
fn leaving_at_any_step_marks_the_server_onboarded_and_closes_for_good() {
    let workspace = workspace_dir();
    for step in 0..3 {
        let mut application = offered_onboarding(workspace.path());
        for _ in 0..step {
            press(&mut application, KeyCode::Enter);
        }
        assert!(screen(&application).contains(&format!("Step {} of 3", step + 1)));

        assert_eq!(
            press(&mut application, KeyCode::Esc),
            ApplicationTransition::MarkOnboarded,
            "Esc at step {} leaves the flow",
            step + 1
        );
        assert!(!onboarding_shown(&application), "{}", screen(&application));
        deliver_settings(&mut application, EffectiveSettings::default());
        assert!(
            !onboarding_shown(&application),
            "leaving counts as taking it: {}",
            screen(&application)
        );
    }
}

/// The stream's opening snapshot says what held when it was read, which may
/// be before the reader left: it does not undo what they did since.
#[test]
fn a_stale_word_that_the_server_is_not_onboarded_does_not_reopen_what_the_reader_left() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(false)))
            .expect("hear the stream's opening word on Onboarding"),
        ApplicationTransition::Continue,
        "no second mark is asked for"
    );
    assert!(
        !onboarding_shown(&application),
        "leaving counts as taking it: {}",
        screen(&application)
    );
}

#[test]
fn finishing_the_third_step_marks_the_server_onboarded_and_closes() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

#[test]
fn onboarding_owns_the_keys_while_it_is_open() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    type_terminal_text(&mut application, "draft");
    press(&mut application, KeyCode::Esc);
    let landing = screen(&application);
    assert!(
        !landing.contains("draft"),
        "nothing typed over Onboarding reaches the composer: {landing}"
    );
}

#[test]
fn hearing_the_server_was_onboarded_elsewhere_closes_the_flow() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 2 of 3"));

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(true)))
        .expect("hear that another Client onboarded the Server");
    assert!(!onboarding_shown(&application), "{}", screen(&application));
    deliver_settings(&mut application, EffectiveSettings::default());
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

#[test]
fn a_reconnect_saying_the_server_was_onboarded_meanwhile_closes_the_offered_flow() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 2 of 3"));

    connect(
        &mut application,
        ready_health(fixture_instance_id(), 42_424),
    );
    assert!(
        !onboarding_shown(&application),
        "another Client took it while this one was away: {}",
        screen(&application)
    );
}

#[test]
fn a_retake_the_reader_asked_for_stays_open_whatever_the_server_says() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 2 of 3"));

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(true)))
        .expect("hear that the Server has been onboarded");
    connect(
        &mut application,
        ready_health(fixture_instance_id(), 42_424),
    );
    assert!(
        screen(&application).contains("Step 2 of 3"),
        "the reader asked for it, so it is theirs to leave: {}",
        screen(&application)
    );
}

#[test]
fn slash_onboarding_takes_the_flow_again_from_the_first_step() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 3"));
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 2 of 3"));
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::OnboardingOpen,
        )))
        .expect("invoke onboarding.open");
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "it opens again at the first step: {}",
        screen(&application)
    );
}

#[test]
fn slash_onboarding_opens_over_an_open_session() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    enter_session(&mut application, workspace.path());
    assert!(screen(&application).contains("Initial Prompt"));

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    let over_session = screen(&application);
    assert!(over_session.contains("Step 1 of 3"), "{over_session}");
    assert!(over_session.contains("Initial Prompt"), "{over_session}");
}

fn startup_diagnostic() -> SettingsDiagnostic {
    SettingsDiagnostic {
        severity: SettingsDiagnosticSeverity::Warning,
        file: std::path::PathBuf::from("suru.jsonc"),
        key: Some("transcript.defaultFoldPostures".to_owned()),
        message: "ignored because it is not a known Setting".to_owned(),
    }
}

#[test]
fn the_launch_notice_waits_beneath_onboarding_and_is_read_once_it_closes() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    sidebar: SidebarSettings {
                        initial_visibility: SidebarVisibility::Hidden,
                        ..SidebarSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: vec![startup_diagnostic()],
            },
        )))
        .expect("receive the settings snapshot with a startup diagnostic");
    let notice = |application: &Application| screen(application).contains("see the Log");
    let offered = screen(&application);
    assert!(offered.contains("Step 1 of 3"), "{offered}");
    assert!(notice(&application), "{offered}");

    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Left);
    assert!(
        notice(&application),
        "keys Onboarding takes leave the Notice unread: {}",
        screen(&application)
    );
    press(&mut application, KeyCode::Esc);
    assert!(!onboarding_shown(&application));
    assert!(
        notice(&application),
        "the key that closes Onboarding leaves it standing: {}",
        screen(&application)
    );

    press(&mut application, KeyCode::Char('a'));
    assert!(
        !notice(&application),
        "the next key reads it: {}",
        screen(&application)
    );
}
