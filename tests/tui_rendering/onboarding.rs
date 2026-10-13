//! Onboarding: the three-step pass a Client offers over the Landing of its
//! own Server until that Server has been onboarded, and `/onboarding` takes
//! again whenever the reader likes.

use std::{collections::HashSet, path::Path};

use crate::support::{
    connected_application, deliver_settings, enter_session, fixture_instance_id, model_descriptor,
    ready_health, rendered_application_buffer, rendered_application_rows_at, text_position,
    type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Modifier;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, Health, ModelAvailability, ModelCatalog, Outlook, ProviderCatalogStatus,
        ProviderId, ProviderModelCatalog, ProviderUnavailability, RemoteHealth, RemoteStatus,
        SettingMutation, SettingsDiagnostic, SettingsDiagnosticSeverity, SettingsSnapshot,
        SidebarSettings, SidebarVisibility, TextSelectionCopy, Way,
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

/// The reader left in a Client whose mark never reached the Server, which
/// reconnects still saying it has not been onboarded.
fn left_then_reconnected_unmarked(workspace: &Path) -> Application {
    let mut application = offered_onboarding(workspace);
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    let reconnected = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            unonboarded_health(),
        )))
        .expect("reconnect to a Server that says it is not onboarded");
    assert_ne!(
        reconnected,
        ApplicationTransition::MarkOnboarded,
        "no second mark is asked for"
    );
    deliver_settings(&mut application, EffectiveSettings::default());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(false)))
        .expect("hear the reconnected stream's opening word on Onboarding");
    application
}

#[test]
fn a_reconnect_saying_the_server_is_not_onboarded_does_not_reopen_what_the_reader_left() {
    let workspace = workspace_dir();
    let application = left_then_reconnected_unmarked(workspace.path());
    assert!(
        !onboarding_shown(&application),
        "leaving counts as taking it, so it never returns unasked: {}",
        screen(&application)
    );
}

#[test]
fn slash_onboarding_still_opens_the_flow_the_reader_left() {
    let workspace = workspace_dir();
    let mut application = left_then_reconnected_unmarked(workspace.path());
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "{}",
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
    type_terminal_text(&mut application, "hello");
    press(&mut application, KeyCode::Esc);
    let landing = screen(&application);
    assert!(
        !landing.contains("hello"),
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

/// Settings in which each built-in Provider is enabled as given, in their
/// fixed order: Codex, Copilot, Claude.
fn with_enablement(codex: bool, copilot: bool, claude: bool) -> EffectiveSettings {
    let mut settings = EffectiveSettings::default();
    settings.provider.codex.enabled = codex;
    settings.provider.copilot.enabled = copilot;
    settings.provider.claude.enabled = claude;
    settings
}

/// What an edit's round trip leaves in force, as the Server answers it.
fn settle_mutation(application: &mut Application, settings: EffectiveSettings) {
    application
        .handle_event(ApplicationEvent::SettingMutated(SettingsSnapshot {
            settings,
            pinned: Vec::new(),
            diagnostics: Vec::new(),
        }))
        .expect("receive the settings the edit left in force");
}

/// A catalog naming each Provider by its status alone, which is all a
/// Provider row reads of it.
fn catalog(providers: &[(&str, ProviderCatalogStatus)]) -> ModelCatalog {
    ModelCatalog {
        providers: providers
            .iter()
            .map(|(provider, status)| ProviderModelCatalog {
                provider: ProviderId::new(*provider),
                display_name: (*provider).to_owned(),
                models: Vec::new(),
                status: status.clone(),
            })
            .collect(),
    }
}

/// The Client's own Server pushes the Model Catalog as it now stands, as it
/// does after every connect and whenever a Provider's catalog changes.
fn push_catalog(application: &mut Application, providers: &[(&str, ProviderCatalogStatus)]) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::ModelCatalog(
            catalog(providers),
        )))
        .expect("hear the Model Catalog the Server pushed");
}

fn unavailable(reason: ProviderUnavailability, fix: &str) -> ProviderCatalogStatus {
    ProviderCatalogStatus::Unavailable {
        reason,
        message: "the Provider's own account of it".to_owned(),
        fix: fix.to_owned(),
    }
}

/// Every Provider unavailable, as on a machine with none of their CLIs.
fn nothing_installed() -> [(&'static str, ProviderCatalogStatus); 3] {
    [
        (
            "codex",
            unavailable(
                ProviderUnavailability::NotInstalled,
                "install the Codex CLI from https://developers.openai.com/codex/cli/",
            ),
        ),
        (
            "copilot",
            unavailable(ProviderUnavailability::NotSignedIn, "run `copilot login`"),
        ),
        (
            "claude",
            unavailable(
                ProviderUnavailability::NotInstalled,
                "install the Claude Code CLI from https://code.claude.com/docs/en/setup",
            ),
        ),
    ]
}

/// Every Provider not installed, each with the fix its own check names, as
/// on a machine with none of their CLIs.
fn every_cli_missing() -> [(&'static str, ProviderCatalogStatus); 3] {
    [
        (
            "codex",
            unavailable(
                ProviderUnavailability::NotInstalled,
                "install the Codex CLI from https://developers.openai.com/codex/cli/",
            ),
        ),
        (
            "copilot",
            unavailable(
                ProviderUnavailability::NotInstalled,
                "install the Copilot CLI from \
                 https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli",
            ),
        ),
        (
            "claude",
            unavailable(
                ProviderUnavailability::NotInstalled,
                "install the Claude Code CLI from https://code.claude.com/docs/en/setup",
            ),
        ),
    ]
}

fn mouse(
    application: &mut Application,
    kind: MouseEventKind,
    (column, row): (u16, u16),
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("handle a mouse event")
}

/// Drags across the screen from `start` to `end`, answering what letting go
/// asks for.
fn drag(
    application: &mut Application,
    start: (u16, u16),
    end: (u16, u16),
) -> ApplicationTransition {
    mouse(application, MouseEventKind::Down(MouseButton::Left), start);
    mouse(application, MouseEventKind::Drag(MouseButton::Left), end);
    mouse(application, MouseEventKind::Up(MouseButton::Left), end)
}

/// The line the Providers step draws for `provider`'s row.
fn provider_row(application: &Application, provider: &str) -> String {
    let rows = rendered_application_rows_at(application, 100, 24);
    rows.iter()
        .find(|row| row.contains(&format!("] {provider}")))
        .unwrap_or_else(|| panic!("no row for {provider}: {}", rows.join("\n")))
        .trim_end()
        .to_owned()
}

/// The text of `provider`'s row within the Onboarding box, from its
/// checkbox to the end of what it says.
fn provider_row_text(application: &Application, provider: &str) -> String {
    let row = provider_row(application, provider);
    let start = row
        .find('[')
        .expect("a Provider row begins with its checkbox");
    row[start..].trim_end_matches(['│', ' ']).to_owned()
}

#[test]
fn the_providers_step_lists_every_built_in_provider_ticked_on_a_fresh_install() {
    let workspace = workspace_dir();
    let application = offered_onboarding(workspace.path());
    let rows = rendered_application_rows_at(&application, 100, 24);
    let order = ["Codex", "Copilot", "Claude"].map(|provider| {
        rows.iter()
            .position(|row| row.contains(&format!("[x] {provider}")))
            .unwrap_or_else(|| panic!("{provider} begins ticked: {}", rows.join("\n")))
    });
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "in the fixed built-in order: {}",
        rows.join("\n")
    );
    assert!(
        provider_row(&application, "Codex").contains("› [x] Codex"),
        "the cursor begins on the first: {}",
        screen(&application)
    );
}

#[test]
fn space_writes_the_provider_under_the_cursor_through_its_enablement_at_once() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());

    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(false),
        }),
        "Space turns the Provider under the cursor off"
    );
    assert!(
        provider_row(&application, "Codex").contains("[x] Codex"),
        "the row waits on the Server rather than showing what the file has not taken"
    );
    settle_mutation(&mut application, with_enablement(false, true, true));
    assert!(
        provider_row(&application, "Codex").contains("[ ] Codex"),
        "and follows what the edit left in force: {}",
        screen(&application)
    );

    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCopilotEnabled {
            value: Some(false),
        }),
        "Down moves the cursor to the next Provider"
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Up);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderClaudeEnabled {
            value: Some(false),
        }),
        "the cursor wraps past either end: {}",
        screen(&application)
    );
    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(true),
        }),
        "and Space turns a Provider back on"
    );
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "toggling moves no step"
    );
}

#[test]
fn each_provider_row_shows_its_catalog_status() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · checking",
        "nothing heard yet is said as such"
    );

    push_catalog(
        &mut application,
        &[
            ("codex", ProviderCatalogStatus::Fresh),
            ("copilot", ProviderCatalogStatus::Refreshing),
            (
                "claude",
                ProviderCatalogStatus::Warning {
                    message: "Claude Code 1.0 lacks the newest permission modes".to_owned(),
                },
            ),
        ],
    );
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available"
    );
    assert_eq!(
        provider_row_text(&application, "Copilot"),
        "[x] Copilot · refreshing"
    );
    assert_eq!(
        provider_row_text(&application, "Claude"),
        "[x] Claude · warning"
    );
    assert!(
        screen(&application).contains("Claude Code 1.0 lacks the newest permission modes"),
        "beneath a warning, what it warns of: {}",
        screen(&application)
    );

    push_catalog(
        &mut application,
        &[(
            "codex",
            ProviderCatalogStatus::Stale {
                message: "the newest refresh failed".to_owned(),
            },
        )],
    );
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available",
        "a catalog still serving what it last read is still available"
    );

    push_catalog(
        &mut application,
        &[(
            "copilot",
            ProviderCatalogStatus::Failed {
                message: "the Model listing timed out".to_owned(),
            },
        )],
    );
    assert_eq!(
        provider_row_text(&application, "Copilot"),
        "[x] Copilot · error"
    );
    assert!(
        screen(&application).contains("the Model listing timed out"),
        "beneath a failed read, what failed: {}",
        screen(&application)
    );
}

#[test]
fn an_unavailable_provider_shows_its_reason_and_the_whole_fix_beneath() {
    const INSTALL_PAGE: &str =
        "https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli";
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(
        &mut application,
        &[
            (
                "codex",
                unavailable(ProviderUnavailability::NotSignedIn, "run `codex login`"),
            ),
            (
                "copilot",
                unavailable(
                    ProviderUnavailability::NotInstalled,
                    &format!("install the Copilot CLI from {INSTALL_PAGE}"),
                ),
            ),
            (
                "claude",
                unavailable(
                    ProviderUnavailability::IncompatibleVersion,
                    "update the Claude Code CLI",
                ),
            ),
        ],
    );

    // Wide enough for the box to hold the install page's address on a line.
    let rows = rendered_application_rows_at(&application, 140, 30);
    let screen = rows.join("\n");
    for (provider, reason, fix) in [
        ("Codex", "not signed in", "run `codex login`"),
        ("Copilot", "not installed", "install the Copilot CLI from"),
        (
            "Claude",
            "incompatible version",
            "update the Claude Code CLI",
        ),
    ] {
        let row = rows
            .iter()
            .position(|row| row.contains(&format!("] {provider}")))
            .unwrap_or_else(|| panic!("{provider} has a row: {screen}"));
        assert!(
            rows[row].contains(&format!("[x] {provider} · {reason} ")),
            "{screen}"
        );
        assert!(
            rows[row + 1].contains(fix),
            "what fixes {provider} stands beneath its row: {screen}"
        );
    }
    assert!(
        screen.contains(INSTALL_PAGE),
        "an install page is drawn whole, never cut short: {screen}"
    );
}

#[test]
fn a_disabled_provider_claims_no_availability() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_enablement(true, false, true));
    push_catalog(
        &mut application,
        &[
            ("codex", ProviderCatalogStatus::Fresh),
            ("copilot", ProviderCatalogStatus::Disabled),
            ("claude", ProviderCatalogStatus::Fresh),
        ],
    );
    assert_eq!(
        provider_row_text(&application, "Copilot"),
        "[ ] Copilot · disabled"
    );
}

#[test]
fn the_recheck_key_issues_the_all_providers_refresh_and_its_answer_reaches_the_rows() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let footer = screen(&application);
    assert!(
        footer.contains("r Re-check"),
        "the key is taught at the foot: {footer}"
    );

    let ApplicationTransition::RecheckProviders { recheck } =
        press(&mut application, KeyCode::Char('r'))
    else {
        panic!(
            "r re-reads every Provider's Availability: {}",
            screen(&application)
        );
    };
    assert!(
        screen(&application).contains("Re-checking every Provider…"),
        "and says so while it does: {}",
        screen(&application)
    );

    application
        .handle_event(ApplicationEvent::ProvidersRechecked {
            recheck,
            result: Ok(catalog(&[
                ("codex", ProviderCatalogStatus::Fresh),
                (
                    "copilot",
                    unavailable(ProviderUnavailability::NotSignedIn, "run `copilot login`"),
                ),
                ("claude", ProviderCatalogStatus::Refreshing),
            ])),
        })
        .expect("receive what the re-check found");
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available",
        "{}",
        screen(&application)
    );
    assert_eq!(
        provider_row_text(&application, "Claude"),
        "[x] Claude · refreshing"
    );
    assert!(
        !screen(&application).contains("Re-checking"),
        "{}",
        screen(&application)
    );
}

#[test]
fn a_recheck_the_server_did_not_answer_says_so_and_leaves_the_rows() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let ApplicationTransition::RecheckProviders { recheck } =
        press(&mut application, KeyCode::Char('r'))
    else {
        panic!("r re-reads every Provider's Availability");
    };
    application
        .handle_event(ApplicationEvent::ProvidersRechecked {
            recheck,
            result: Err("the Server did not answer".to_owned()),
        })
        .expect("hear that the re-check failed");

    let failed = screen(&application);
    assert!(
        failed.contains("Could not re-check the Providers: the Server did not answer"),
        "{failed}"
    );
    assert!(!failed.contains("Re-checking"), "{failed}");
    assert!(
        provider_row_text(&application, "Copilot").contains("not signed in"),
        "the rows keep what was last heard: {failed}"
    );
}

/// Onboarding's rows are the Client's own Server's whichever Outlook is
/// turned, so `r` re-reads that Server's Providers over a Remote too — and
/// as a request of the flow's own, beside rather than in place of the
/// listing the Model picker is waiting on from the Remote, which still
/// settles.
#[test]
fn the_recheck_reads_the_clients_own_server_over_a_remote_and_its_listing_still_settles() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    turn_outlook(&mut application, KeyCode::Down);
    let ApplicationTransition::ListModels(listing) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("open the Model picker")
    else {
        panic!("the picker asks the Remote for its Models");
    };
    assert_eq!(listing.outlook(), &Outlook::Remote("studio".to_owned()));
    press(&mut application, KeyCode::Esc);

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    push_catalog(&mut application, &nothing_installed());
    assert!(
        screen(&application).contains("r Re-check"),
        "{}",
        screen(&application)
    );
    let ApplicationTransition::RecheckProviders { recheck } =
        press(&mut application, KeyCode::Char('r'))
    else {
        panic!(
            "r re-reads the Client's own Server's Providers over a Remote: {}",
            screen(&application)
        );
    };
    application
        .handle_event(ApplicationEvent::ProvidersRechecked {
            recheck,
            result: Ok(catalog(&[("codex", ProviderCatalogStatus::Fresh)])),
        })
        .expect("receive what the re-check found");
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available",
        "{}",
        screen(&application)
    );

    application
        .handle_event(ApplicationEvent::ModelsRefreshed {
            request: listing,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "studio-model",
                        "Studio Model",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("receive the Remote's Models");
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("open the Model picker again");
    assert!(
        screen(&application).contains("Studio Model"),
        "the Remote's listing settled as the picker asked for it: {}",
        screen(&application)
    );
}

/// A terminal too short for every Provider with its fix beneath it shows a
/// window over them that follows the cursor, so the Provider Space would
/// toggle is always one on screen, while why Next is locked and the keys at
/// the foot keep their place beneath it.
#[test]
fn a_short_screen_keeps_the_lock_line_and_keys_and_follows_the_cursor() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &every_cli_missing());
    let short =
        |application: &Application| rendered_application_rows_at(application, 80, 16).join("\n");

    let first = short(&application);
    assert!(first.contains("› [x] Codex · not installed"), "{first}");
    assert!(
        !first.contains("Claude"),
        "the last Provider is out of view to begin with: {first}"
    );
    for kept in ["Next is locked:", "Enter Next", "Esc Leave"] {
        assert!(first.contains(kept), "{kept:?} keeps its place: {first}");
    }

    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    let last = short(&application);
    assert!(
        last.contains("› [x] Claude · not installed"),
        "the cursor brings the last Provider into view: {last}"
    );
    assert!(
        last.contains("https://code.claude.com/docs/en/setup"),
        "with its fix: {last}"
    );
    for kept in ["Next is locked:", "Enter Next", "Esc Leave"] {
        assert!(last.contains(kept), "{kept:?} keeps its place: {last}");
    }
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderClaudeEnabled {
            value: Some(false),
        }),
        "Space toggles the Provider on screen under the cursor"
    );

    press(&mut application, KeyCode::Down);
    assert!(
        short(&application).contains("› [x] Codex · not installed"),
        "and wrapping to the first brings it back: {}",
        short(&application)
    );
}

/// On a narrow terminal the keys at the foot give up the step's own hints
/// before Next and Leave, and why Next is locked wraps rather than being cut.
#[test]
fn a_narrow_screen_keeps_next_and_leave_and_the_whole_lock_line() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    push_catalog(&mut application, &every_cli_missing());

    let rows = rendered_application_rows_at(&application, 40, 30);
    let narrow = rows.join("\n");
    let foot = rows
        .iter()
        .find(|row| row.contains("Esc Leave"))
        .unwrap_or_else(|| panic!("Leave is taught: {narrow}"));
    assert!(foot.contains("Enter Next"), "and Next beside it: {narrow}");
    assert!(
        !foot.contains('…'),
        "hints give way whole rather than being cut: {narrow}"
    );
    let lock = rows
        .iter()
        .position(|row| row.contains("Next is locked:"))
        .unwrap_or_else(|| panic!("why Next is locked is drawn: {narrow}"));
    let explanation = rows[lock..=lock + 1]
        .iter()
        .map(|row| row.trim_matches(|c: char| c == '│' || c.is_whitespace()))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        explanation.contains("Next is locked: no enabled Provider is available."),
        "whole, over as many rows as it takes: {narrow}"
    );
}

/// A terminal too small to draw the row under the cursor acts on no
/// Provider the reader cannot see: Space toggles nothing and Enter passes
/// nothing until a frame has room for that row. Why Next is locked is given
/// whole before any row, the gap beneath the header giving way first.
#[test]
fn a_tiny_screen_acts_on_no_provider_it_does_not_draw() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    push_catalog(&mut application, &every_cli_missing());

    let tiny = rendered_application_rows_at(&application, 40, 8).join("\n");
    assert!(tiny.contains("Step 1 of 3"), "{tiny}");
    assert!(
        tiny.contains("Next is locked: no enabled") && tiny.contains("Provider is available."),
        "why Next is locked is given whole: {tiny}"
    );
    assert!(
        !tiny.contains("Codex"),
        "with no room left for a row: {tiny}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue,
        "Space toggles no Provider the screen does not draw"
    );

    let roomier = rendered_application_rows_at(&application, 40, 14).join("\n");
    assert!(roomier.contains("› [x] Codex"), "{roomier}");
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(false),
        }),
        "and toggles the one it does"
    );

    push_catalog(&mut application, &[("codex", ProviderCatalogStatus::Fresh)]);
    let headless = rendered_application_rows_at(&application, 40, 6).join("\n");
    assert!(!headless.contains("Codex"), "{headless}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows_at(&application, 40, 8)
            .join("\n")
            .contains("Step 1 of 3"),
        "Enter passes no step whose rows the screen did not draw"
    );
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Step 2 of 3"),
        "and passes it once they are drawn: {}",
        screen(&application)
    );
}

/// However narrow the terminal, every step's keys at the foot teach how to
/// go on and how to leave, whole rather than cut short.
#[test]
fn a_narrow_terminal_teaches_next_or_finish_and_leave_whole_at_every_step() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &[("codex", ProviderCatalogStatus::Fresh)]);
    for (step, go_on) in [(1, "Next"), (2, "Next"), (3, "Finish")] {
        let rows = rendered_application_rows_at(&application, 28, 24);
        let shown = rows.join("\n");
        assert!(shown.contains(&format!("Step {step} of 3")), "{shown}");
        let foot = rows
            .iter()
            .find(|row| row.contains("Esc Leave"))
            .unwrap_or_else(|| panic!("step {step} teaches Leave whole: {shown}"));
        assert!(
            foot.contains(&format!("{go_on} · Esc Leave")) && !foot.contains('…'),
            "step {step} teaches {go_on} beside it: {shown}"
        );
        press(&mut application, KeyCode::Enter);
    }
}

/// What fixes an unavailable Provider, wrapped beneath its row, copies as it
/// was written: the wrap that broke an install page's address across rows,
/// and the indent that set it beneath the row, are the step's layout rather
/// than the text (ADR-0020).
#[test]
fn a_wrapped_fix_on_the_providers_step_copies_as_one_unbroken_line() {
    const INSTALL_PAGE: &str =
        "https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli";
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = TextSelectionCopy::Release;
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    push_catalog(&mut application, &every_cli_missing());
    press(&mut application, KeyCode::Down);

    let buffer = rendered_application_buffer(&application, 60, 30);
    let start = text_position(&buffer, "install the Copilot CLI from");
    let address = text_position(&buffer, "https://docs.github.com/en/");
    let tail = text_position(&buffer, "install-copilot-cli");
    assert!(
        start.1 < address.1 && address.1 < tail.1,
        "the address wraps onto a row of its own and breaks across another"
    );
    let end = (tail.0 + "install-copilot-cli".len() as u16 - 1, tail.1);
    assert_eq!(
        drag(&mut application, start, end),
        ApplicationTransition::CopyToClipboard(
            format!("install the Copilot CLI from {INSTALL_PAGE}").into()
        ),
        "the whole fix copies as written"
    );
    let highlighted = rendered_application_buffer(&application, 60, 30);
    assert!(
        !highlighted[(address.0 - 1, address.1)]
            .modifier
            .contains(Modifier::REVERSED),
        "the indent it is drawn beneath its row from is not lit"
    );
    assert_eq!(
        drag(&mut application, address, end),
        ApplicationTransition::CopyToClipboard(INSTALL_PAGE.into()),
        "and the address alone copies unbroken"
    );

    // Why Next is locked, wrapped beneath the rows, copies as written too.
    let narrow = rendered_application_buffer(&application, 40, 30);
    let lock = text_position(&narrow, "Next is locked:");
    let tail = text_position(&narrow, "available.");
    assert!(lock.1 < tail.1, "the explanation wraps onto a second row");
    assert_eq!(
        drag(
            &mut application,
            lock,
            (tail.0 + "available.".len() as u16 - 1, tail.1)
        ),
        ApplicationTransition::CopyToClipboard(
            "Next is locked: no enabled Provider is available.".into()
        ),
        "the explanation copies as one line"
    );
}

#[test]
fn next_stays_locked_while_no_enabled_provider_is_available_and_unlocks_once_one_is() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());

    let locked = screen(&application);
    assert!(
        locked.contains("Next is locked: no enabled Provider is available"),
        "a line under the rows says why: {locked}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application).contains("Step 1 of 3"),
        "Enter moves nowhere: {}",
        screen(&application)
    );

    let [codex, copilot, _] = nothing_installed();
    push_catalog(
        &mut application,
        &[codex, copilot, ("claude", ProviderCatalogStatus::Fresh)],
    );
    let unlocked = screen(&application);
    assert!(!unlocked.contains("Next is locked"), "{unlocked}");
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Step 2 of 3"),
        "one available Provider is enough: {}",
        screen(&application)
    );
}

/// A machine with none of the Providers' CLIs is where a first-time user is
/// likeliest to begin, and the box makes room for every fix and for why Next
/// is locked rather than cutting either short.
#[test]
fn every_fix_and_the_lock_line_are_drawn_with_no_provider_installed() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &every_cli_missing());

    let shown = screen(&application);
    for drawn in [
        "https://developers.openai.com/codex/cli/",
        "install the Copilot CLI from",
        "https://code.claude.com/docs/en/setup",
        "Next is locked: no enabled Provider is available.",
        "Esc Leave",
    ] {
        assert!(shown.contains(drawn), "{drawn:?} is drawn: {shown}");
    }
}

#[test]
fn next_stays_locked_while_every_provider_is_disabled() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_enablement(false, false, false));
    push_catalog(
        &mut application,
        &[
            ("codex", ProviderCatalogStatus::Disabled),
            ("copilot", ProviderCatalogStatus::Disabled),
            ("claude", ProviderCatalogStatus::Disabled),
        ],
    );

    let locked = screen(&application);
    assert!(
        locked.contains("Next is locked: enable at least one Provider"),
        "{locked}"
    );
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 3"));

    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(true),
        })
    );
    settle_mutation(&mut application, with_enablement(true, false, false));
    assert!(
        !screen(&application).contains("Next is locked"),
        "an enabled Provider not found unavailable opens the way: {}",
        screen(&application)
    );
}

#[test]
fn an_available_provider_that_is_disabled_does_not_unlock_next() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_enablement(true, false, false));
    let [codex, ..] = nothing_installed();
    push_catalog(
        &mut application,
        &[
            codex,
            ("copilot", ProviderCatalogStatus::Fresh),
            ("claude", ProviderCatalogStatus::Fresh),
        ],
    );
    assert!(
        screen(&application).contains("Next is locked: no enabled Provider is available"),
        "{}",
        screen(&application)
    );
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 3"));
}

#[test]
fn a_warning_or_refreshing_provider_does_not_lock_next() {
    for passing in [
        ProviderCatalogStatus::Warning {
            message: "an older CLI than Suru prefers".to_owned(),
        },
        ProviderCatalogStatus::Refreshing,
    ] {
        let workspace = workspace_dir();
        let mut application = offered_onboarding(workspace.path());
        let [codex, copilot, _] = nothing_installed();
        push_catalog(&mut application, &[codex, copilot, ("claude", passing)]);
        assert!(
            !screen(&application).contains("Next is locked"),
            "{}",
            screen(&application)
        );
        press(&mut application, KeyCode::Enter);
        assert!(
            screen(&application).contains("Step 2 of 3"),
            "{}",
            screen(&application)
        );
    }
}

#[test]
fn going_back_to_the_providers_step_finds_it_as_left() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Left);
    assert!(
        provider_row(&application, "Copilot").contains("› [x] Copilot"),
        "{}",
        screen(&application)
    );
}

#[test]
fn slash_onboarding_shows_providers_turned_off_in_the_settings_panel() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());

    press_with(&mut application, KeyCode::Char('x'), KeyModifiers::CONTROL);
    press(&mut application, KeyCode::Char(','));
    press(&mut application, KeyCode::Right);
    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCopilotEnabled {
            value: Some(false),
        }),
        "the settings panel turns Copilot off"
    );
    settle_mutation(&mut application, with_enablement(true, false, true));
    press(&mut application, KeyCode::Esc);

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 3"));
    assert!(provider_row(&application, "Codex").contains("[x] Codex"));
    assert!(
        provider_row(&application, "Copilot").contains("[ ] Copilot"),
        "{}",
        screen(&application)
    );
    assert!(provider_row(&application, "Claude").contains("[x] Claude"));
    assert!(
        provider_row(&application, "Codex").contains("› [x] Codex"),
        "the cursor begins on the first row again"
    );
}
