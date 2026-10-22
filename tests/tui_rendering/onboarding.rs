//! Onboarding: the four-step pass a Client offers over the Landing of its
//! own Server until that Server has been onboarded, and `/onboarding` takes
//! again whenever the reader likes.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::support::{
    MutatesSetting, connected_application, deliver_settings, enter_session, fixture_instance_id,
    model_descriptor, ready_health, rendered_application_buffer, rendered_application_rows_at,
    text_position, type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Modifier;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, ChildDirectory, DerivationErrand, DerivationSettings, DirectoryListing,
        DirectorySourceControl, EffectiveSettings, Health, ModelAvailability, ModelCatalog,
        ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId,
        ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue, Outlook,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, ProviderUnavailability,
        RemoteHealth, RemoteStatus, SettingMutation, SettingsDiagnostic,
        SettingsDiagnosticSeverity, SettingsSnapshot, SidebarSettings, SidebarVisibility,
        TextSelectionCopy, Way, WorkspaceSettings,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, DirectoryListingId,
        SemanticCommandId, SettingRequest, TerminalFacts,
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
    rendered_application_rows_at(application, 108, 24).join("\n")
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
    screen(application).contains(" of 4")
}

/// Moves focus down to the Confirm row at the foot of the step on show, as
/// the reader does with the arrows.
fn focus_confirm(application: &mut Application) {
    for _ in 0..32 {
        if screen(application).contains("› Confirm") {
            return;
        }
        press(application, KeyCode::Down);
    }
    panic!("no Confirm row came under focus: {}", screen(application));
}

/// Confirms the Providers or Workspace Roots step from its Confirm row,
/// answering what that asks for.
fn confirm_step(application: &mut Application) -> ApplicationTransition {
    focus_confirm(application);
    press(application, KeyCode::Enter)
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
    assert!(offered.contains("Step 1 of 4"), "{offered}");
    assert!(offered.contains("Providers"), "{offered}");
    assert!(
        offered.contains("Agent unavailable"),
        "over the Landing, whose status still shows beneath: {offered}"
    );

    let mut settings_first = Application::new(workspace.path(), TerminalFacts::default());
    deliver_settings(&mut settings_first, EffectiveSettings::default());
    assert!(!onboarding_shown(&settings_first), "nor are settings alone");
    connect(&mut settings_first, unonboarded_health());
    assert!(screen(&settings_first).contains("Step 1 of 4"));
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
        screen(&application).contains("Step 1 of 4"),
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
        screen(&application).contains("Step 1 of 4"),
        "back on its own Server, the Client offers it: {}",
        screen(&application)
    );
}

#[test]
fn the_header_counts_the_steps_as_next_and_back_move_between_them() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let first = screen(&application);
    assert!(first.contains("Step 1 of 4"), "{first}");
    assert!(first.contains("Confirm"), "{first}");
    assert!(!first.contains("Back"), "the first step has none: {first}");
    assert_eq!(
        press(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert!(screen(&application).contains("Step 1 of 4"));

    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    let second = screen(&application);
    assert!(second.contains("Step 2 of 4"), "{second}");
    assert!(second.contains("Derivation"), "{second}");
    assert!(second.contains("Back"), "{second}");

    press(&mut application, KeyCode::Left);
    assert!(screen(&application).contains("Step 1 of 4"));
    confirm_step(&mut application);
    pass_the_derivation_step(&mut application, EffectiveSettings::default());
    let third = screen(&application);
    assert!(third.contains("Step 3 of 4 · Workspace Roots"), "{third}");
    focus_confirm(&mut application);
    assert!(screen(&application).contains("Enter Next"), "{third}");

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "Roots left as they stand pin nothing"
    );
    let fourth = screen(&application);
    assert!(fourth.contains("Step 4 of 4 · Icons"), "{fourth}");
    assert!(
        fourth.contains("Finish"),
        "the last step finishes: {fourth}"
    );
    press_with(&mut application, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(screen(&application).contains("Step 3 of 4"));
    press(&mut application, KeyCode::Left);
    assert!(screen(&application).contains("Step 2 of 4"));
}

#[test]
fn leaving_at_any_step_marks_the_server_onboarded_and_closes_for_good() {
    let workspace = workspace_dir();
    for step in 0..4 {
        let mut application = offered_onboarding(workspace.path());
        if step >= 1 {
            confirm_step(&mut application);
        }
        if step >= 2 {
            pass_the_derivation_step(&mut application, EffectiveSettings::default());
        }
        if step >= 3 {
            confirm_step(&mut application);
        }
        assert!(screen(&application).contains(&format!("Step {} of 4", step + 1)));

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
        screen(&application).contains("Step 1 of 4"),
        "{}",
        screen(&application)
    );
}

#[test]
fn finishing_the_last_step_marks_the_server_onboarded_and_closes() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    let derivation = press(&mut application, KeyCode::Enter);
    assert_eq!(derivation, derivation_pin(DerivationErrand::FollowSession));
    settle_mutation(&mut application, &derivation, EffectiveSettings::default());
    assert!(screen(&application).contains("Step 3 of 4"));
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    assert!(screen(&application).contains("Step 4 of 4"));
    press(&mut application, KeyCode::Down);
    let pin = finish_with(&mut application, true);
    assert_eq!(
        acknowledge(&mut application, pin, true),
        ApplicationTransition::MarkOnboarded,
        "the mark follows the pin once it lands"
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
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 2 of 4"));

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
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 2 of 4"));

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
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 2 of 4"));

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(true)))
        .expect("hear that the Server has been onboarded");
    connect(
        &mut application,
        ready_health(fixture_instance_id(), 42_424),
    );
    assert!(
        screen(&application).contains("Step 2 of 4"),
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
    assert!(screen(&application).contains("Step 1 of 4"));
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 2 of 4"));
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
        screen(&application).contains("Step 1 of 4"),
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
    assert!(over_session.contains("Step 1 of 4"), "{over_session}");
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
    assert!(offered.contains("Step 1 of 4"), "{offered}");
    assert!(notice(&application), "{offered}");

    confirm_step(&mut application);
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

/// The round trip of `edit`, a transition asking for a Setting mutation: the
/// Server's stream pushes `settings`, what the edit left in force, and its
/// answer to the edit says it landed.
fn settle_mutation(
    application: &mut Application,
    edit: &ApplicationTransition,
    settings: EffectiveSettings,
) {
    deliver_settings(application, settings);
    answer(application, request_of(edit));
}

/// The Server's answer to the edit sent under `request`: that it landed.
/// What it left in force reaches the Client on the Server's stream instead.
fn answer(application: &mut Application, request: SettingRequest) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::SettingMutated { request })
        .expect("hear the edit landed")
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
    let rows = rendered_application_rows_at(application, 108, 24);
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
    let rows = rendered_application_rows_at(&application, 108, 24);
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

    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) }),
        "Space turns the Provider under the cursor off"
    );
    assert!(
        provider_row(&application, "Codex").contains("[x] Codex"),
        "the row waits on the Server rather than showing what the file has not taken"
    );
    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(false, true, true),
    );
    assert!(
        provider_row(&application, "Codex").contains("[ ] Codex"),
        "and follows what the edit left in force: {}",
        screen(&application)
    );

    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderCopilotEnabled { value: Some(false) }),
        "Down moves the cursor to the next Provider"
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert!(
        screen(&application).contains("› Confirm"),
        "the Confirm row stands beneath the last Provider: {}",
        screen(&application)
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Up);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderClaudeEnabled { value: Some(false) }),
        "the cursor wraps past either end, through the Confirm row: {}",
        screen(&application)
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(true) }),
        "and Space turns a Provider back on"
    );
    assert!(
        screen(&application).contains("Step 1 of 4"),
        "toggling moves no step"
    );
}

#[test]
fn enter_toggles_the_provider_under_the_cursor_as_space_does() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let footer = screen(&application);
    assert!(
        footer.contains("r Re-check · ↑↓ Move · Enter Toggle · Esc Leave"),
        "the keys say what Enter does to the row under the cursor: {footer}"
    );

    let toggle = press(&mut application, KeyCode::Enter);
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) }),
        "Enter turns the Provider under the cursor off"
    );
    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(false, true, true),
    );
    assert!(
        screen(&application).contains("Step 1 of 4"),
        "rather than moving on: {}",
        screen(&application)
    );
}

#[test]
fn space_or_enter_on_the_confirm_row_passes_the_providers_step() {
    let workspace = workspace_dir();
    for key in [KeyCode::Char(' '), KeyCode::Enter] {
        let mut application = offered_onboarding(workspace.path());
        press(&mut application, KeyCode::Up);
        let on_confirm = screen(&application);
        assert!(
            on_confirm.contains("› Confirm"),
            "Up from the first Provider wraps to the Confirm row: {on_confirm}"
        );
        assert!(
            on_confirm.contains("Enter Next"),
            "where Enter goes on: {on_confirm}"
        );
        assert_eq!(
            press(&mut application, key),
            ApplicationTransition::Continue,
            "{key:?} on the Confirm row toggles nothing"
        );
        assert!(
            screen(&application).contains("Step 2 of 4"),
            "{key:?} on the Confirm row passes the step: {}",
            screen(&application)
        );
    }
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

/// Asks for a re-check of every Provider, answering the request its answer
/// must name.
fn recheck(application: &mut Application) -> uuid::Uuid {
    let ApplicationTransition::RecheckProviders { recheck } =
        press(application, KeyCode::Char('r'))
    else {
        panic!("r re-reads every Provider's Availability");
    };
    recheck
}

/// The Client's own Server answers the re-check `recheck`.
fn answer_recheck(
    application: &mut Application,
    recheck: uuid::Uuid,
    result: Result<ModelCatalog, String>,
) {
    application
        .handle_event(ApplicationEvent::ProvidersRechecked { recheck, result })
        .expect("receive the re-check's answer");
}

fn codex_available() -> ModelCatalog {
    let [_, copilot, claude] = nothing_installed();
    catalog(&[("codex", ProviderCatalogStatus::Fresh), copilot, claude])
}

/// Only the newest re-check answers for the rows: one superseded before it
/// answered says nothing of them or of Next, however late it lands.
#[test]
fn only_the_newest_recheck_answers_for_the_rows() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let first = recheck(&mut application);
    let second = recheck(&mut application);

    answer_recheck(&mut application, second, Ok(codex_available()));
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available"
    );
    answer_recheck(&mut application, first, Ok(catalog(&nothing_installed())));
    answer_recheck(&mut application, first, Err("too late".to_owned()));
    let after = screen(&application);
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available",
        "the superseded answer does not undo the newest: {after}"
    );
    assert!(!after.contains("Next is locked"), "{after}");
    assert!(!after.contains("Could not re-check"), "{after}");
    confirm_step(&mut application);
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "{}",
        screen(&application)
    );
}

/// A re-check the flow was left before it answered moves nothing: the rows,
/// taken again, read what the Server last said itself.
#[test]
fn a_recheck_answered_after_leaving_the_flow_moves_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let asked = recheck(&mut application);
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );

    answer_recheck(&mut application, asked, Ok(codex_available()));
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · not installed",
        "{}",
        screen(&application)
    );
}

/// A re-check asked of the Server before the Client reconnected to it is no
/// longer awaited: what that Server is now says itself, so the old answer
/// moves nothing.
#[test]
fn a_recheck_asked_before_a_reconnect_moves_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let asked = recheck(&mut application);

    connect(&mut application, unonboarded_health());
    assert!(
        !screen(&application).contains("Re-checking"),
        "{}",
        screen(&application)
    );
    answer_recheck(&mut application, asked, Ok(codex_available()));
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · not installed",
        "{}",
        screen(&application)
    );
}

/// What the Server pushes is its newest word, so a re-check asked before a
/// push landed gives way to that push.
#[test]
fn a_push_after_the_recheck_was_asked_outranks_its_answer() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &nothing_installed());
    let asked = recheck(&mut application);

    let [_, copilot, claude] = nothing_installed();
    push_catalog(
        &mut application,
        &[("codex", ProviderCatalogStatus::Fresh), copilot, claude],
    );
    answer_recheck(&mut application, asked, Ok(catalog(&nothing_installed())));
    let after = screen(&application);
    assert_eq!(
        provider_row_text(&application, "Codex"),
        "[x] Codex · available",
        "{after}"
    );
    assert!(
        !after.contains("Re-checking"),
        "the re-check is settled all the same: {after}"
    );
}

/// Space acts on the Provider under the cursor only where the last frame
/// drew that Provider, so keys taken together — a move and then Space,
/// before any frame shows where the move went — toggle nothing unseen.
#[test]
fn space_after_a_move_no_frame_has_shown_toggles_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &every_cli_missing());
    let short = rendered_application_rows_at(&application, 80, 16).join("\n");
    assert!(!short.contains("Claude"), "{short}");

    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Up);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue,
        "the cursor moved past the Confirm row to Claude, which no frame has drawn"
    );

    let shown = rendered_application_rows_at(&application, 80, 16).join("\n");
    assert!(shown.contains("› [x] Claude"), "{shown}");
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderClaudeEnabled { value: Some(false) }),
        "once a frame has drawn it, Space toggles it"
    );
}

/// A terminal shrunk below what Suru draws in shows no Provider at all, so
/// Space toggles none until a frame draws one again.
#[test]
fn space_on_a_terminal_too_small_to_draw_toggles_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let normal = screen(&application);
    assert!(normal.contains("› [x] Codex"), "{normal}");

    let skipped = rendered_application_rows_at(&application, 20, 10).join("\n");
    assert!(!skipped.contains("Codex"), "{skipped}");
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );

    assert!(screen(&application).contains("› [x] Codex"));
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) })
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
    for kept in ["Next is locked:", "Confirm", "Enter Toggle", "Esc Leave"] {
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
    for kept in ["Next is locked:", "Confirm", "Enter Toggle", "Esc Leave"] {
        assert!(last.contains(kept), "{kept:?} keeps its place: {last}");
    }
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        MutatesSetting(SettingMutation::ProviderClaudeEnabled { value: Some(false) }),
        "Space toggles the Provider on screen under the cursor"
    );

    press(&mut application, KeyCode::Down);
    assert!(
        short(&application).contains("› Confirm"),
        "the Confirm row beneath them takes the cursor next: {}",
        short(&application)
    );
    press(&mut application, KeyCode::Down);
    assert!(
        short(&application).contains("› [x] Codex · not installed"),
        "and wrapping to the first brings it back: {}",
        short(&application)
    );
}

/// On a narrow terminal the keys at the foot give up the step's own hints
/// before Enter and Leave, and why Next is locked wraps rather than being
/// cut.
#[test]
fn a_narrow_screen_keeps_enter_and_leave_and_the_whole_lock_line() {
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
    assert!(
        foot.contains("Enter Toggle"),
        "and Enter beside it: {narrow}"
    );
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

/// A terminal too small to draw the row under the cursor acts on no row the
/// reader cannot see: Space toggles nothing, and Enter on the Confirm row
/// passes nothing, until a frame has room for that row. Why Next is locked
/// is given whole before any row, the gap beneath the header giving way
/// first.
#[test]
fn a_tiny_screen_acts_on_no_provider_it_does_not_draw() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    push_catalog(&mut application, &every_cli_missing());

    let tiny = rendered_application_rows_at(&application, 40, 9).join("\n");
    assert!(tiny.contains("Step 1 of 4"), "{tiny}");
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
    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) }),
        "and toggles the one it does"
    );
    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(false, true, true),
    );

    push_catalog(&mut application, &[("codex", ProviderCatalogStatus::Fresh)]);
    press(&mut application, KeyCode::Up);
    let headless = rendered_application_rows_at(&application, 40, 6).join("\n");
    assert!(!headless.contains("Confirm"), "{headless}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows_at(&application, 40, 8)
            .join("\n")
            .contains("Step 1 of 4"),
        "Enter passes no step whose Confirm row the screen did not draw"
    );
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "and passes it once they are drawn: {}",
        screen(&application)
    );
}

/// However narrow the terminal, every step's keys at the foot teach how to
/// go on and how to leave, whole rather than cut short: from the Confirm row
/// on the steps that have one.
#[test]
fn a_narrow_terminal_teaches_next_or_finish_and_leave_whole_at_every_step() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    push_catalog(&mut application, &[("codex", ProviderCatalogStatus::Fresh)]);
    for (step, go_on) in [(1, "Next"), (2, "Next"), (3, "Next"), (4, "Finish")] {
        if step == 1 || step == 3 {
            focus_confirm(&mut application);
        }
        let rows = rendered_application_rows_at(&application, 28, 24);
        let shown = rows.join("\n");
        assert!(shown.contains(&format!("Step {step} of 4")), "{shown}");
        let foot = rows
            .iter()
            .find(|row| row.contains("Esc Leave"))
            .unwrap_or_else(|| panic!("step {step} teaches Leave whole: {shown}"));
        assert!(
            foot.contains(&format!("{go_on} · Esc Leave")) && !foot.contains('…'),
            "step {step} teaches {go_on} beside it: {shown}"
        );
        if step == 2 {
            pass_the_derivation_step(&mut application, EffectiveSettings::default());
        } else {
            press(&mut application, KeyCode::Enter);
        }
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
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application).contains("Step 1 of 4"),
        "Confirm moves nowhere: {}",
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
        screen(&application).contains("Step 2 of 4"),
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
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 1 of 4"));

    // Down from the Confirm row wraps to the first Provider.
    press(&mut application, KeyCode::Down);
    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(true) })
    );
    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(true, false, false),
    );
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
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 1 of 4"));
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
        confirm_step(&mut application);
        assert!(
            screen(&application).contains("Step 2 of 4"),
            "{}",
            screen(&application)
        );
    }
}

#[test]
fn going_back_to_the_providers_step_finds_it_as_left() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    confirm_step(&mut application);
    press(&mut application, KeyCode::Left);
    assert!(
        screen(&application).contains("› Confirm"),
        "the cursor stands where the step was confirmed from: {}",
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
    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCopilotEnabled { value: Some(false) }),
        "the settings panel turns Copilot off"
    );
    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(true, false, true),
    );
    press(&mut application, KeyCode::Esc);

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 4"));
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

/// The Model the Derivation step's tests pin, which carries Model Options so
/// a pin can tell the options the reader chose from the ones they left be.
fn cheap_model() -> suru::protocol::ModelDescriptor {
    let mut cheap = model_descriptor(
        "codex",
        "gpt-5-mini",
        "GPT-5 Mini",
        false,
        ModelAvailability::Available,
    );
    cheap.options = vec![
        ModelOptionDescriptor {
            id: ModelOptionId::new("reasoning_effort"),
            label: "Reasoning".to_owned(),
            description: None,
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices: ["low", "high"]
                    .into_iter()
                    .map(|choice| ModelOptionChoice {
                        id: ModelOptionChoiceId::new(choice),
                        label: choice.to_uppercase(),
                        description: None,
                        availability: ModelAvailability::Available,
                    })
                    .collect(),
                default: ModelOptionChoiceId::new("low"),
            },
        },
        ModelOptionDescriptor {
            id: ModelOptionId::new("fast"),
            label: "Speed".to_owned(),
            description: None,
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Toggle { default: false },
        },
    ];
    cheap
}

/// The Model Catalog as the Server pushes it: Codex offering a configurable
/// Model and its default, Copilot offering one Model, and Claude, which is
/// not installed, still listing the Model it last offered.
fn pushed_catalog() -> ModelCatalog {
    ModelCatalog {
        providers: vec![
            ProviderModelCatalog {
                provider: ProviderId::new("codex"),
                display_name: "Codex".to_owned(),
                models: vec![
                    cheap_model(),
                    model_descriptor(
                        "codex",
                        "gpt-5",
                        "GPT-5",
                        true,
                        ModelAvailability::Available,
                    ),
                ],
                status: ProviderCatalogStatus::Fresh,
            },
            ProviderModelCatalog {
                provider: ProviderId::new("copilot"),
                display_name: "Copilot".to_owned(),
                models: vec![model_descriptor(
                    "copilot",
                    "copilot-model",
                    "Copilot Model",
                    true,
                    ModelAvailability::Available,
                )],
                status: ProviderCatalogStatus::Fresh,
            },
            unavailable_claude(),
        ],
    }
}

fn unavailable_claude() -> ProviderModelCatalog {
    ProviderModelCatalog {
        provider: ProviderId::new("claude"),
        display_name: "Claude".to_owned(),
        models: vec![model_descriptor(
            "claude",
            "claude-model",
            "Claude Model",
            true,
            ModelAvailability::Available,
        )],
        status: ProviderCatalogStatus::Unavailable {
            reason: ProviderUnavailability::NotInstalled,
            message: "claude was not found".to_owned(),
            fix: "install Claude Code".to_owned(),
        },
    }
}

fn deliver_catalog(application: &mut Application, catalog: ModelCatalog) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::ModelCatalog(
            catalog,
        )))
        .expect("receive the Model Catalog the Server pushes");
}

/// Settings deriving Titles, Icons, and branch names with `errand`, and with
/// Copilot turned off.
fn settings_deriving_with(errand: DerivationErrand) -> EffectiveSettings {
    let mut settings = EffectiveSettings {
        derivation: DerivationSettings { errand },
        ..EffectiveSettings::default()
    };
    settings.provider.copilot.enabled = false;
    settings
}

/// A fresh Server's Client on Onboarding's Derivation step, having heard
/// `settings` and then the catalog the Server pushes after them.
fn on_the_derivation_step(workspace: &Path, settings: EffectiveSettings) -> Application {
    let mut application = attached_to_a_fresh_server(workspace);
    deliver_settings(&mut application, settings);
    deliver_catalog(&mut application, pushed_catalog());
    confirm_step(&mut application);
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "{}",
        screen(&application)
    );
    application
}

/// The row of the Derivation step Row Focus stands on.
fn focused_row(application: &Application) -> String {
    let rows = rendered_application_rows_at(application, 108, 24);
    rows.iter()
        .find(|row| row.contains('›'))
        .unwrap_or_else(|| panic!("the Derivation step focused nothing: {rows:#?}"))
        .trim()
        .to_owned()
}

/// What confirming the Derivation step on `errand` asks the Server to pin.
fn derivation_pin(errand: DerivationErrand) -> MutatesSetting {
    MutatesSetting(SettingMutation::DerivationErrand {
        value: Some(errand),
    })
}

fn cheap_selection(options: Vec<ModelOptionSelection>) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5-mini"),
        options,
    }
}

#[test]
fn the_derivation_step_offers_the_providers_default_and_off_above_the_models_of_usable_providers() {
    let workspace = workspace_dir();
    let application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    let rows = rendered_application_rows_at(&application, 108, 24);
    let position = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row reads {needle:?}: {rows:#?}"))
    };
    assert!(
        position("Provider's default") < position("  Off")
            && position("  Off") < position("Codex")
            && position("Codex") < position("GPT-5 Mini"),
        "the two named values come first, above the Provider and its Models: {rows:#?}"
    );
    assert!(rows.iter().any(|row| row.contains("GPT-5 ·")), "{rows:#?}");
    let shown = rows.join("\n");
    assert!(
        !shown.contains("Copilot"),
        "a Provider the user turned off offers nothing: {shown}"
    );
    assert!(
        !shown.contains("Claude"),
        "nor does one that is unavailable: {shown}"
    );
    assert!(
        !shown.contains("session"),
        "the named value reads as the Provider's default, not the word it is spelled with: {shown}"
    );
}

#[test]
fn the_derivation_step_follows_enablement_and_availability_as_they_change() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );

    let mut catalog = pushed_catalog();
    catalog.providers[2] = ProviderModelCatalog {
        status: ProviderCatalogStatus::Fresh,
        ..unavailable_claude()
    };
    deliver_catalog(&mut application, catalog);
    let installed = screen(&application);
    assert!(
        installed.contains("Claude") && installed.contains("Claude Model"),
        "Claude, installed since, offers its Model: {installed}"
    );

    let mut settings = settings_deriving_with(DerivationErrand::FollowSession);
    settings.provider.codex.enabled = false;
    deliver_settings(&mut application, settings);
    let turned_off = screen(&application);
    assert!(
        !turned_off.contains("Codex") && !turned_off.contains("GPT-5"),
        "Codex, turned off on the step before, offers nothing: {turned_off}"
    );
    assert!(turned_off.contains("Claude Model"), "{turned_off}");
}

#[test]
fn confirming_the_providers_default_or_off_pins_that_value_and_moves_on_to_icons() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    assert!(
        focused_row(&application).contains("Provider's default"),
        "{}",
        focused_row(&application)
    );
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::FollowSession));
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    assert!(screen(&application).contains("Step 3 of 4"));

    press(&mut application, KeyCode::Left);
    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("Off"),
        "{}",
        focused_row(&application)
    );
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::Off));
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Off),
    );
    assert!(screen(&application).contains("Step 3 of 4"));
}

#[test]
fn confirming_a_model_without_model_options_pins_it_and_moves_on_to_icons() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("GPT-5 ·"),
        "{}",
        focused_row(&application)
    );
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(
        pin,
        derivation_pin(DerivationErrand::Pinned(AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5"),
            options: Vec::new(),
        }))
    );
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Pinned(AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5"),
            options: Vec::new(),
        })),
    );
    assert!(screen(&application).contains("Step 3 of 4"));
}

#[test]
fn a_model_with_model_options_opens_them_over_the_step_and_pins_only_what_was_chosen() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    for _ in 0..3 {
        press(&mut application, KeyCode::Down);
    }
    assert!(
        focused_row(&application).contains("GPT-5 Mini"),
        "{}",
        focused_row(&application)
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "a Model with Model Options has them chosen before anything is pinned"
    );
    let options = screen(&application);
    assert!(options.contains("Model Options"), "{options}");
    assert!(
        options.contains("Reasoning · Provider default")
            && options.contains("Speed · Provider default"),
        "nothing is pinned yet, so every option follows the Model's default: {options}"
    );

    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );
    let cancelled = screen(&application);
    assert!(
        cancelled.contains("Step 2 of 4") && !cancelled.contains("Model Options"),
        "cancelling the options leaves the reader on the step: {cancelled}"
    );

    press(&mut application, KeyCode::Enter);
    // Reasoning's choices, under "Provider default": Low, then High.
    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);
    let pin = press_with(&mut application, KeyCode::Enter, KeyModifiers::CONTROL);
    assert_eq!(
        pin,
        derivation_pin(DerivationErrand::Pinned(cheap_selection(vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            }
        ]))),
        "the chosen Reasoning is pinned, and the untouched Speed follows the Model's default"
    );
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Pinned(cheap_selection(vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
        ]))),
    );
    let moved_on = screen(&application);
    assert!(
        moved_on.contains("Step 3 of 4") && !moved_on.contains("Model Options"),
        "choosing the options answers the step: {moved_on}"
    );
}

#[test]
fn taking_onboarding_again_rests_row_focus_on_the_derivation_value_in_force() {
    let workspace = workspace_dir();
    let pinned = cheap_selection(vec![ModelOptionSelection {
        id: ModelOptionId::new("fast"),
        value: ModelOptionValue::Toggle { enabled: true },
    }]);
    for (errand, focused) in [
        (DerivationErrand::FollowSession, "Provider's default"),
        (DerivationErrand::Off, "Off"),
        (DerivationErrand::Pinned(pinned.clone()), "GPT-5 Mini"),
    ] {
        let mut application = connected_application(workspace.path());
        deliver_settings(&mut application, settings_deriving_with(errand.clone()));
        deliver_catalog(&mut application, pushed_catalog());
        type_terminal_text(&mut application, "/onboarding");
        press(&mut application, KeyCode::Enter);
        confirm_step(&mut application);

        let row = focused_row(&application);
        assert!(
            row.contains(focused) && row.contains("current"),
            "{errand:?} has Row Focus and is marked as the value in force: {row}"
        );
    }

    let mut application = connected_application(workspace.path());
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::Pinned(pinned)),
    );
    deliver_catalog(&mut application, pushed_catalog());
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    confirm_step(&mut application);
    press(&mut application, KeyCode::Enter);
    let options = screen(&application);
    assert!(
        options.contains("Speed · On") && options.contains("Reasoning · Provider default"),
        "the options open on what the pin already holds: {options}"
    );
}

/// Confirms the retry row Row Focus stands on, which asks the Client's own
/// Server to re-read every Provider, answering the request its answer must
/// name.
fn retry(application: &mut Application) -> uuid::Uuid {
    let transition = press(application, KeyCode::Enter);
    let ApplicationTransition::RecheckProviders { recheck } = transition else {
        panic!("a retry row re-reads every Provider of the Client's own Server: {transition:?}");
    };
    recheck
}

#[test]
fn a_provider_whose_listing_failed_is_retried_from_the_derivation_step() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    let mut catalog = pushed_catalog();
    catalog.providers[0].models.clear();
    catalog.providers[0].status = ProviderCatalogStatus::Failed {
        message: "codex timed out".to_owned(),
    };
    deliver_catalog(&mut application, catalog);
    confirm_step(&mut application);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("Retry Codex: codex timed out"),
        "{}",
        focused_row(&application)
    );

    let recheck = retry(&mut application);
    assert!(screen(&application).contains("Step 2 of 4"));
    answer_recheck(&mut application, recheck, Ok(pushed_catalog()));
    let listed = screen(&application);
    assert!(
        listed.contains("GPT-5 Mini") && !listed.contains("Retry Codex"),
        "the step lists what the retry found: {listed}"
    );
}

#[test]
fn a_warning_or_refreshing_provider_still_offers_its_models_on_the_derivation_step() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    let mut catalog = pushed_catalog();
    catalog.providers[0].status = ProviderCatalogStatus::Warning {
        message: "codex is older than Suru prefers".to_owned(),
    };
    catalog.providers[2] = ProviderModelCatalog {
        status: ProviderCatalogStatus::Refreshing,
        ..unavailable_claude()
    };
    deliver_catalog(&mut application, catalog);
    confirm_step(&mut application);

    let shown = screen(&application);
    assert!(
        shown.contains("Codex") && shown.contains("GPT-5 Mini"),
        "a Provider with a warning is usable, so its Models are offered: {shown}"
    );
    assert!(
        shown.contains("Claude · refreshing") && shown.contains("Claude Model"),
        "as are those of one whose catalog is being read again: {shown}"
    );
}

/// The Local Server's catalog with Claude's listing failed, so its row is a
/// retry.
fn local_catalog_with_claude_failing() -> ModelCatalog {
    let mut catalog = pushed_catalog();
    catalog.providers[2] = ProviderModelCatalog {
        models: Vec::new(),
        status: ProviderCatalogStatus::Failed {
            message: "claude timed out".to_owned(),
        },
        ..unavailable_claude()
    };
    catalog
}

/// A Client viewing the Remote studio, whose own Server lists Claude's
/// listing as failed, on the Derivation step of the Onboarding it asked for,
/// with Row Focus on Claude's retry row.
fn retrying_on_the_derivation_step_viewing_studio(workspace: &Path) -> Application {
    let mut application = connected_application(workspace);
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    deliver_catalog(&mut application, local_catalog_with_claude_failing());
    turn_outlook(&mut application, KeyCode::Down);
    let studio = Outlook::Remote("studio".to_owned());
    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: studio.clone(),
            event: ManagedEvent::ModelCatalog(ModelCatalog {
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
            }),
        })
        .expect("hear the Remote's catalog");

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    confirm_step(&mut application);
    let shown = screen(&application);
    assert!(shown.contains("Step 2 of 4"), "{shown}");
    assert!(
        shown.contains("GPT-5 Mini") && !shown.contains("Studio Model"),
        "the Models offered are the Client's own Server's, whose Setting is pinned: {shown}"
    );

    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("Retry Claude"),
        "{}",
        focused_row(&application)
    );
    application
}

#[test]
fn the_derivation_step_lists_and_retries_the_clients_own_server_while_a_remote_is_viewed() {
    let workspace = workspace_dir();
    let mut application = retrying_on_the_derivation_step_viewing_studio(workspace.path());
    let recheck = retry(&mut application);
    let mut listed = pushed_catalog();
    listed.providers[2].status = ProviderCatalogStatus::Fresh;
    answer_recheck(&mut application, recheck, Ok(listed));
    let relisted = screen(&application);
    assert!(
        relisted.contains("Claude Model") && !relisted.contains("Studio Model"),
        "what the retry found is the step's: {relisted}"
    );

    for _ in 0..3 {
        press(&mut application, KeyCode::Down);
    }
    assert!(
        focused_row(&application).contains("GPT-5 ·"),
        "{}",
        focused_row(&application)
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        derivation_pin(DerivationErrand::Pinned(AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5"),
            options: Vec::new(),
        }))
    );
}

/// A fresh Server's Client with the Model Options of GPT-5 Mini open over
/// Onboarding's Derivation step.
fn choosing_options_on_the_derivation_step(workspace: &Path) -> Application {
    let mut application = on_the_derivation_step(
        workspace,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    for _ in 0..3 {
        press(&mut application, KeyCode::Down);
    }
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Model Options"),
        "{}",
        screen(&application)
    );
    application
}

fn apply_options(application: &mut Application) -> ApplicationTransition {
    press_with(application, KeyCode::Enter, KeyModifiers::CONTROL)
}

#[test]
fn the_options_onboarding_opened_close_with_it_when_another_client_takes_it() {
    let workspace = workspace_dir();
    let mut application = choosing_options_on_the_derivation_step(workspace.path());

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(true)))
        .expect("hear that another Client onboarded the Server");
    let after = screen(&application);
    assert!(
        !onboarding_shown(&application) && !after.contains("Model Options"),
        "nothing the flow offered stands to be pinned: {after}"
    );
    assert_ne!(
        apply_options(&mut application),
        derivation_pin(DerivationErrand::Pinned(cheap_selection(Vec::new())))
    );
}

#[test]
fn the_options_onboarding_opened_close_with_it_when_it_is_left() {
    let workspace = workspace_dir();
    let mut application = choosing_options_on_the_derivation_step(workspace.path());

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::OnboardingLeave,
            )))
            .expect("leave Onboarding"),
        ApplicationTransition::MarkOnboarded
    );
    let after = screen(&application);
    assert!(
        !onboarding_shown(&application) && !after.contains("Model Options"),
        "{after}"
    );
}

#[test]
fn applying_options_opened_elsewhere_does_not_answer_the_derivation_step() {
    let workspace = workspace_dir();
    let mut application = choosing_options_on_the_derivation_step(workspace.path());
    press(&mut application, KeyCode::Esc);
    assert!(screen(&application).contains("Step 2 of 4"));

    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptions,
        )))
        .expect("open the Agent's Model Options")
    else {
        panic!("the Agent's Model Options read the catalog first");
    };
    let mut cheap_by_default = cheap_model();
    cheap_by_default.is_default = true;
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".to_owned(),
                    models: vec![cheap_by_default],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("receive the catalog");
    assert!(
        screen(&application).contains("Model Options"),
        "{}",
        screen(&application)
    );

    let applied = apply_options(&mut application);
    assert!(
        !matches!(applied, ApplicationTransition::MutateSetting { .. }),
        "the Agent's options pin nothing: {applied:?}"
    );
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "and leave the Derivation step unanswered: {}",
        screen(&application)
    );
}

#[test]
fn options_for_a_provider_turned_off_or_found_unavailable_meanwhile_refuse_to_apply() {
    let workspace = workspace_dir();
    let mut application = choosing_options_on_the_derivation_step(workspace.path());
    let mut settings = settings_deriving_with(DerivationErrand::FollowSession);
    settings.provider.codex.enabled = false;
    deliver_settings(&mut application, settings);
    assert!(
        screen(&application).contains("unavailable"),
        "{}",
        screen(&application)
    );
    assert_eq!(
        apply_options(&mut application),
        ApplicationTransition::Continue,
        "a Model of a Provider turned off cannot be pinned"
    );

    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    let mut catalog = pushed_catalog();
    catalog.providers[0].status = ProviderCatalogStatus::Unavailable {
        reason: ProviderUnavailability::NotSignedIn,
        message: "codex is signed out".to_owned(),
        fix: "run `codex login`".to_owned(),
    };
    deliver_catalog(&mut application, catalog);
    assert_eq!(
        apply_options(&mut application),
        ApplicationTransition::Continue,
        "nor one of a Provider found unavailable"
    );

    deliver_catalog(&mut application, pushed_catalog());
    let pin = apply_options(&mut application);
    assert_eq!(
        pin,
        derivation_pin(DerivationErrand::Pinned(cheap_selection(Vec::new()))),
        "and once it is usable again the options apply"
    );
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Pinned(cheap_selection(Vec::new()))),
    );
    assert!(screen(&application).contains("Step 3 of 4"));
}

#[test]
fn an_acknowledged_pin_moves_the_mark_of_the_value_in_force() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    press(&mut application, KeyCode::Down);
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::Off));
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Off),
    );

    press(&mut application, KeyCode::Left);
    let rows = rendered_application_rows_at(&application, 108, 24);
    let row = |label: &str| {
        rows.iter()
            .find(|row| row.contains(label))
            .unwrap_or_else(|| panic!("no row reads {label:?}: {rows:#?}"))
            .clone()
    };
    assert!(row("Off").contains("current"), "{rows:#?}");
    assert!(!row("Provider's default").contains("current"), "{rows:#?}");
}

/// The Codex catalog the viewport test lists: `before` Models named to sort
/// above Model 01, then Models 01 to 12.
fn codex_listing(before: usize) -> ModelCatalog {
    let models = (1..=before)
        .map(|index| (format!("alpha-{index:02}"), format!("Alpha {index:02}")))
        .chain((1..=12).map(|index| (format!("m{index:02}"), format!("Model {index:02}"))))
        .map(|(id, name)| {
            model_descriptor("codex", &id, &name, false, ModelAvailability::Available)
        })
        .collect();
    ModelCatalog {
        providers: vec![ProviderModelCatalog {
            provider: ProviderId::new("codex"),
            display_name: "Codex".to_owned(),
            models,
            status: ProviderCatalogStatus::Fresh,
        }],
    }
}

/// The rows the Derivation step draws, from its first to its last.
fn derivation_rows(application: &Application) -> Vec<String> {
    let rows = rendered_application_rows_at(application, 108, 24);
    let first = rows
        .iter()
        .position(|row| row.contains("Which Agent Selection derives"))
        .expect("the Derivation step is drawn")
        + 2;
    let foot = rows
        .iter()
        .position(|row| row.contains("Esc Leave"))
        .expect("the keys are drawn");
    // Each row reads the Sidebar, the main view's margin, then the box.
    rows[first..foot]
        .iter()
        .filter_map(|row| row.split('│').nth(2))
        .map(|row| row.trim_end().to_owned())
        .filter(|row| !row.is_empty())
        .collect()
}

#[test]
fn the_window_holds_while_rows_listed_above_row_focus_leave_it_in_view() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    deliver_catalog(&mut application, codex_listing(0));
    confirm_step(&mut application);
    for _ in 0..6 {
        press(&mut application, KeyCode::Down);
    }
    let walked = derivation_rows(&application);
    assert!(
        walked.iter().any(|row| row.starts_with("› Model 05")),
        "{walked:#?}"
    );

    deliver_catalog(&mut application, codex_listing(1));
    let held = derivation_rows(&application);
    assert_eq!(
        held.first(),
        walked.first(),
        "a row listed above Row Focus that leaves it in view moves no window: {held:#?}"
    );
    assert!(
        held.iter().any(|row| row.starts_with("› Model 05")),
        "{held:#?}"
    );

    deliver_catalog(&mut application, codex_listing(13));
    let moved = derivation_rows(&application);
    assert!(
        moved
            .last()
            .is_some_and(|row| row.starts_with("› Model 05")),
        "rows that would push Row Focus out of view move the window only as far as keeps it \
         in view: {moved:#?}"
    );
}

#[test]
fn a_failed_retry_of_the_clients_own_server_shows_its_retry_rows_while_a_remote_is_viewed() {
    let workspace = workspace_dir();
    let mut application = retrying_on_the_derivation_step_viewing_studio(workspace.path());
    let recheck = retry(&mut application);
    answer_recheck(
        &mut application,
        recheck,
        Err("the Server did not answer".to_owned()),
    );

    let failed = screen(&application);
    assert!(failed.contains("Step 2 of 4"), "{failed}");
    assert!(
        failed.contains("Retry Codex: the Server did not answer")
            && failed.contains("Retry Claude: the Server did not answer"),
        "the failure is the Client's own Server's, retried from the step as the picker would: \
         {failed}"
    );
    assert!(!failed.contains("Studio Model"), "{failed}");
}

#[test]
fn the_agents_options_asked_for_earlier_do_not_replace_the_derivation_steps() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptions,
        )))
        .expect("ask for the Agent's Model Options")
    else {
        panic!("the Agent's Model Options read the catalog first");
    };
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    deliver_catalog(&mut application, pushed_catalog());
    confirm_step(&mut application);
    for _ in 0..3 {
        press(&mut application, KeyCode::Down);
    }
    press(&mut application, KeyCode::Enter);
    // Reasoning's choices, under "Provider default": Low, then High.
    press(&mut application, KeyCode::Enter);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Reasoning · HIGH"),
        "{}",
        screen(&application)
    );

    let mut cheap_by_default = cheap_model();
    cheap_by_default.is_default = true;
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".to_owned(),
                    models: vec![cheap_by_default],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("receive the catalog the Agent's options asked for");
    assert!(
        screen(&application).contains("Reasoning · HIGH"),
        "the step's options, and what the reader chose in them, stand: {}",
        screen(&application)
    );
    let pin = apply_options(&mut application);
    assert_eq!(
        pin,
        derivation_pin(DerivationErrand::Pinned(cheap_selection(vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            }
        ])))
    );
    settle_mutation(
        &mut application,
        &pin,
        settings_deriving_with(DerivationErrand::Pinned(cheap_selection(vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
        ]))),
    );
    assert!(screen(&application).contains("Step 3 of 4"));
}

#[test]
fn a_retry_from_the_derivation_step_leaves_a_remote_listing_in_flight_to_settle() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    deliver_catalog(&mut application, local_catalog_with_claude_failing());
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
    confirm_step(&mut application);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("Retry Claude"),
        "{}",
        focused_row(&application)
    );
    retry(&mut application);

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

/// The pushed catalog once Claude is installed, which the Derivation step
/// lists whole on a screen of its usual size.
fn claude_installed() -> ModelCatalog {
    let mut catalog = pushed_catalog();
    catalog.providers[2].status = ProviderCatalogStatus::Fresh;
    catalog
}

/// Claude offering one Model, and Codex Models 01 to 12.
fn claude_and_codex_listing() -> ModelCatalog {
    let mut catalog = codex_listing(0);
    catalog.providers.insert(
        0,
        ProviderModelCatalog {
            status: ProviderCatalogStatus::Fresh,
            ..unavailable_claude()
        },
    );
    catalog
}

/// A fresh Server's Client on the Derivation step listing `catalog`, with a
/// re-check of every Provider asked from the Providers step and not yet
/// answered.
fn awaiting_a_recheck_on_the_derivation_step(
    workspace: &Path,
    catalog: ModelCatalog,
) -> (Application, uuid::Uuid) {
    let mut application = attached_to_a_fresh_server(workspace);
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    deliver_catalog(&mut application, catalog);
    let recheck = recheck(&mut application);
    confirm_step(&mut application);
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "{}",
        screen(&application)
    );
    (application, recheck)
}

#[test]
fn retry_rows_a_failure_lists_above_row_focus_leave_it_in_view() {
    let workspace = workspace_dir();
    let (mut application, recheck) =
        awaiting_a_recheck_on_the_derivation_step(workspace.path(), claude_and_codex_listing());
    press(&mut application, KeyCode::Up);
    let walked = derivation_rows(&application);
    assert!(
        walked
            .last()
            .is_some_and(|row| row.starts_with("› Model 12")),
        "{walked:#?}"
    );

    answer_recheck(&mut application, recheck, Err("timed out".to_owned()));
    let failed = derivation_rows(&application);
    assert!(
        failed
            .last()
            .is_some_and(|row| row.starts_with("› Model 12")),
        "a retry row listed above Row Focus moves the window only as far as keeps it in view: \
         {failed:#?}"
    );
}

#[test]
fn a_listing_failure_stands_through_settings_until_a_catalog_supersedes_it() {
    let workspace = workspace_dir();
    let (mut application, recheck) =
        awaiting_a_recheck_on_the_derivation_step(workspace.path(), claude_installed());
    answer_recheck(&mut application, recheck, Err("timed out".to_owned()));
    let failed = screen(&application);
    assert!(
        failed.contains("Retry Claude: timed out") && failed.contains("Retry Codex: timed out"),
        "{failed}"
    );

    let mut settings = settings_deriving_with(DerivationErrand::FollowSession);
    settings.provider.codex.enabled = false;
    deliver_settings(&mut application, settings);
    let turned_off = screen(&application);
    assert!(
        turned_off.contains("Retry Claude: timed out"),
        "a snapshot of the settings says nothing of the listing: {turned_off}"
    );
    assert!(
        !turned_off.contains("Codex"),
        "though a Provider turned off since still leaves the list: {turned_off}"
    );

    deliver_catalog(&mut application, claude_installed());
    let superseded = screen(&application);
    assert!(
        !superseded.contains("Retry Claude"),
        "a catalog heard since is the newer word: {superseded}"
    );
}

#[test]
fn an_unrelated_settings_snapshot_leaves_row_focus_on_a_retry_row_and_the_window_still() {
    let workspace = workspace_dir();
    let (mut application, recheck) =
        awaiting_a_recheck_on_the_derivation_step(workspace.path(), claude_and_codex_listing());
    answer_recheck(&mut application, recheck, Err("timed out".to_owned()));
    press(&mut application, KeyCode::Up);
    let focused = derivation_rows(&application);
    assert!(
        focused
            .last()
            .is_some_and(|row| row.starts_with("› Retry Codex: timed out")),
        "{focused:#?}"
    );

    let mut settings = settings_deriving_with(DerivationErrand::FollowSession);
    settings.text_selection.copy = TextSelectionCopy::Manual;
    deliver_settings(&mut application, settings);
    assert_eq!(
        derivation_rows(&application),
        focused,
        "Row Focus stays on its retry row, and the window where it stood"
    );
    assert!(
        matches!(
            press(&mut application, KeyCode::Enter),
            ApplicationTransition::RecheckProviders { .. }
        ),
        "and Enter retries rather than pinning: {}",
        screen(&application)
    );
}

/// The glyphs the Icons step's sample draws, in order: Icon Catalog glyphs
/// from both font families the Catalog draws on — Material Design's bug,
/// rocket, and flask, Devicons' Rust and Python — then the fixed Codex,
/// Copilot, and Claude glyphs, the folder, and the branch.
const SAMPLE_GLYPHS: [char; 10] = [
    '\u{f00e4}',
    '\u{f14de}',
    '\u{f0093}',
    '\u{e7a8}',
    '\u{e73c}',
    '\u{ec81}',
    '\u{ec1e}',
    '\u{ec82}',
    '\u{ea83}',
    '\u{ec6f}',
];

const NERD_FONT_NOTE: &str = "To get Icons, install a Nerd Font and select it in your terminal, \
                              then turn on Show icons in /settings.";

/// Walks the flow on to its last step, drawn, the Server accepting the
/// derivation pin with the default Settings in force.
fn on_the_icons_step(application: &mut Application) {
    on_the_icons_step_holding(application, EffectiveSettings::default());
}

/// Walks the flow on to its last step, drawn, the Server accepting the
/// derivation pin with `settings` in force and the Workspace Roots passed as
/// they stand.
fn on_the_icons_step_holding(application: &mut Application, settings: EffectiveSettings) {
    confirm_step(application);
    pass_the_derivation_step(application, settings);
    assert_eq!(
        confirm_step(application),
        ApplicationTransition::Continue,
        "the Roots in force pin nothing"
    );
    let shown = screen(application);
    assert!(shown.contains("Step 4 of 4 · Icons"), "{shown}");
}

/// Confirms the Derivation step on the row under Row Focus, the Server
/// accepting the pin it sends with `settings` in force, which moves the
/// flow on.
fn pass_the_derivation_step(application: &mut Application, settings: EffectiveSettings) {
    let pin = press(application, KeyCode::Enter);
    assert!(
        matches!(pin, ApplicationTransition::MutateSetting { .. }),
        "the Derivation step pins its answer: {pin:?}"
    );
    settle_mutation(application, &pin, settings);
}

/// A Client that has been onboarded, holding `settings`, with the flow taken
/// again from `/onboarding` and walked on to its last step.
fn retaking_the_icons_step(workspace: &Path, settings: EffectiveSettings) -> Application {
    let mut application = connected_application(workspace);
    deliver_settings(&mut application, settings.clone());
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    on_the_icons_step_holding(&mut application, settings);
    application
}

fn with_show_icons(show_icons: bool) -> EffectiveSettings {
    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = show_icons;
    settings
}

/// The pin Finish sends on the Icons step, ahead of any mark.
fn pins_show_icons(show_icons: bool) -> MutatesSetting {
    MutatesSetting(SettingMutation::AppearanceShowIcons {
        value: Some(show_icons),
    })
}

/// The request a transition asking for a Setting mutation sends it under.
fn request_of(transition: &ApplicationTransition) -> SettingRequest {
    match transition {
        ApplicationTransition::MutateSetting { request, .. } => *request,
        other => panic!("a Setting mutation is asked for: {other:?}"),
    }
}

/// Presses Finish on the Icons step, which pins Show icons at `show_icons`,
/// answering the request the pin is sent under.
fn finish_with(application: &mut Application, show_icons: bool) -> SettingRequest {
    let finish = press(application, KeyCode::Enter);
    assert_eq!(finish, pins_show_icons(show_icons));
    request_of(&finish)
}

/// The Server's answer to the mutation sent under `request`: the Settings
/// it left in force, with Show icons pinned at `show_icons`.
fn acknowledge(
    application: &mut Application,
    request: SettingRequest,
    show_icons: bool,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: with_show_icons(show_icons),
                pinned: vec!["appearance.showIcons".to_owned()],
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the Settings the edit left in force");
    answer(application, request)
}

/// The Server's refusal of the mutation sent under `request`.
fn refuse(
    application: &mut Application,
    request: SettingRequest,
    error: &str,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::SettingMutationFailed {
            request,
            error: error.to_owned(),
        })
        .expect("hear the edit was refused")
}

#[test]
fn the_icons_step_draws_its_sample_glyphs_with_show_icons_off() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_show_icons(false));
    on_the_icons_step(&mut application);

    let rows = rendered_application_rows_at(&application, 108, 24);
    let sample = rows
        .iter()
        .find(|row| row.contains(SAMPLE_GLYPHS[0]))
        .unwrap_or_else(|| panic!("the sample is drawn: {}", rows.join("\n")));
    let mut rest = sample.as_str();
    for glyph in SAMPLE_GLYPHS {
        let at = rest
            .find(glyph)
            .unwrap_or_else(|| panic!("{glyph:?} is drawn in its place: {sample}"));
        rest = &rest[at + glyph.len_utf8()..];
    }
    let question = rows
        .iter()
        .position(|row| row.contains("How does this terminal draw the row below?"))
        .unwrap_or_else(|| panic!("the step asks how they draw: {}", rows.join("\n")));
    let answers = rows
        .iter()
        .position(|row| row.contains("They draw as icons"))
        .unwrap_or_else(|| panic!("{}", rows.join("\n")));
    let drawn_at = rows.iter().position(|row| row == sample).unwrap();
    assert!(
        question < drawn_at && drawn_at < answers,
        "the sample stands between the question and its answers: {}",
        rows.join("\n")
    );
    assert!(
        rows[answers + 1].contains("They draw as boxes or blanks"),
        "{}",
        rows.join("\n")
    );
}

#[test]
fn nothing_is_chosen_on_a_first_run_and_finish_waits_for_a_choice() {
    let workspace = workspace_dir();
    // Even a Config Document already showing Icons leaves the first answer
    // to the reader, who must look before they answer.
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_show_icons(true));
    on_the_icons_step_holding(&mut application, with_show_icons(true));
    let shown = screen(&application);
    assert!(shown.contains("  They draw as icons"), "{shown}");
    assert!(shown.contains("  They draw as boxes or blanks"), "{shown}");
    assert!(!shown.contains("› They draw"), "nothing is chosen: {shown}");

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "Finish does nothing until a row is chosen"
    );
    assert!(screen(&application).contains("Step 4 of 4"));

    press(&mut application, KeyCode::Down);
    let chosen = screen(&application);
    assert!(chosen.contains("› They draw as icons"), "{chosen}");
    assert!(
        chosen.contains("  They draw as boxes or blanks"),
        "{chosen}"
    );
}

#[test]
fn choosing_the_icons_row_pins_show_icons_on_and_finishes() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    assert!(screen(&application).contains("› They draw as icons"));

    let pin = finish_with(&mut application, true);
    assert_eq!(
        acknowledge(&mut application, pin, true),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
    deliver_settings(&mut application, with_show_icons(true));
    assert!(
        !onboarding_shown(&application),
        "finishing takes it for good: {}",
        screen(&application)
    );
}

#[test]
fn choosing_the_boxes_row_pins_show_icons_off_and_finishes() {
    let workspace = workspace_dir();
    for keys in [&[KeyCode::Down, KeyCode::Down][..], &[KeyCode::Up]] {
        let mut application = offered_onboarding(workspace.path());
        on_the_icons_step(&mut application);
        for key in keys {
            press(&mut application, *key);
        }
        assert!(
            screen(&application).contains("› They draw as boxes or blanks"),
            "{keys:?}: {}",
            screen(&application)
        );

        let pin = finish_with(&mut application, false);
        assert_eq!(
            acknowledge(&mut application, pin, false),
            ApplicationTransition::MarkOnboarded
        );
        assert!(!onboarding_shown(&application), "{}", screen(&application));
    }
}

#[test]
fn the_nerd_font_note_shows_only_while_the_boxes_row_is_chosen() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    let note = "install a Nerd Font";
    assert!(
        !screen(&application).contains(note),
        "{}",
        screen(&application)
    );

    for (row, noted) in [
        ("They draw as icons", false),
        ("They draw as boxes or blanks", true),
        ("They draw as icons", false),
    ] {
        press(&mut application, KeyCode::Down);
        let shown = screen(&application);
        assert!(shown.contains(&format!("› {row}")), "{shown}");
        assert_eq!(shown.contains(note), noted, "{row}: {shown}");
    }
}

#[test]
fn a_retake_puts_the_current_value_under_row_focus() {
    let workspace = workspace_dir();
    for (show_icons, row) in [
        (true, "They draw as icons"),
        (false, "They draw as boxes or blanks"),
    ] {
        let mut application =
            retaking_the_icons_step(workspace.path(), with_show_icons(show_icons));
        let shown = screen(&application);
        assert!(shown.contains(&format!("› {row}")), "{shown}");
        assert_eq!(
            shown.contains("install a Nerd Font"),
            !show_icons,
            "{shown}"
        );

        assert_eq!(
            press(&mut application, KeyCode::Enter),
            pins_show_icons(show_icons),
            "Finish pins the value under Row Focus"
        );
    }
}

#[test]
fn leaving_the_icons_step_pins_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    assert!(screen(&application).contains("› They draw as icons"));

    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

#[test]
fn the_icons_step_teaches_choosing_before_finish() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    let shown = screen(&application);
    assert!(
        shown.contains("↑↓ Choose · Enter Finish · ← Back · Esc Leave"),
        "{shown}"
    );
}

#[test]
fn going_back_from_the_icons_step_finds_the_choice_as_left() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Left);
    press(&mut application, KeyCode::Left);
    assert!(screen(&application).contains("Step 2 of 4"));
    pass_the_derivation_step(&mut application, EffectiveSettings::default());
    press(&mut application, KeyCode::Enter);
    let shown = screen(&application);
    assert!(shown.contains("› They draw as boxes or blanks"), "{shown}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        pins_show_icons(false)
    );
}

/// The note wrapped beneath the answers copies as it was written, rather
/// than as the rows it was drawn on (ADR-0020).
#[test]
fn the_nerd_font_note_wraps_and_copies_as_one_line() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = TextSelectionCopy::Release;
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings.clone());
    on_the_icons_step_holding(&mut application, settings);
    press(&mut application, KeyCode::Up);

    let buffer = rendered_application_buffer(&application, 40, 30);
    let start = text_position(&buffer, "To get Icons,");
    let tail = text_position(&buffer, "/settings.");
    assert!(start.1 < tail.1, "the note wraps onto rows of its own");
    assert_eq!(
        drag(
            &mut application,
            start,
            (tail.0 + "/settings.".len() as u16 - 1, tail.1)
        ),
        ApplicationTransition::CopyToClipboard(NERD_FONT_NOTE.into()),
        "the note copies as one line"
    );
}

/// A screen too short for all the step says keeps the sample and both
/// answers, and the keys at the foot, giving way the rest.
#[test]
fn a_short_screen_keeps_the_sample_and_both_answers() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Up);
    for height in [10, 12, 14] {
        let rows = rendered_application_rows_at(&application, 80, height);
        let shown = rows.join("\n");
        assert!(shown.contains("Step 4 of 4"), "{height}: {shown}");
        assert!(shown.contains(SAMPLE_GLYPHS[9]), "{height}: {shown}");
        assert!(shown.contains("  They draw as icons"), "{height}: {shown}");
        assert!(
            shown.contains("› They draw as boxes or blanks"),
            "{height}: {shown}"
        );
        assert!(shown.contains("Enter Finish"), "{height}: {shown}");
    }
}

/// Finish pins the answer under Row Focus only where the last frame drew
/// it, so a terminal too small to draw the step pins nothing unseen.
#[test]
fn finish_on_a_terminal_too_small_to_draw_pins_nothing() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    assert!(screen(&application).contains("› They draw as icons"));

    let skipped = rendered_application_rows_at(&application, 20, 10).join("\n");
    assert!(!skipped.contains("They draw"), "{skipped}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );

    assert!(screen(&application).contains("› They draw as icons"));
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        pins_show_icons(true)
    );
}

/// Finish sends the pin and waits on the Server's word that it landed
/// before the flow ends, so an answer is never lost behind a mark that
/// closed every Client's flow.
#[test]
fn the_flow_waits_on_the_icons_step_until_the_pin_lands() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    let pin = finish_with(&mut application, true);
    let saving = screen(&application);
    assert!(saving.contains("Step 4 of 4"), "{saving}");
    assert!(saving.contains("Saving your answer…"), "{saving}");

    // Finish, a move, and Back all wait on it.
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Left);
    let waiting = screen(&application);
    assert!(waiting.contains("Step 4 of 4"), "{waiting}");
    assert!(waiting.contains("› They draw as icons"), "{waiting}");
    assert!(waiting.contains("Saving your answer…"), "{waiting}");

    // The Server's answer to another edit is not this pin landing, even
    // where Show icons already stood pinned at the answer, as another
    // Client may have left it.
    assert_eq!(
        acknowledge(&mut application, SettingRequest::new(), true),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application).contains("Saving your answer…"),
        "{}",
        screen(&application)
    );

    assert_eq!(
        acknowledge(&mut application, pin, true),
        ApplicationTransition::MarkOnboarded,
        "the pin's own answer finishes the flow"
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

/// A pin the Server refuses keeps the flow on the Icons step, saying why
/// beneath the answers, and asks no mark: Finish tries the pin again, and
/// Esc leaves without it.
#[test]
fn a_refused_pin_keeps_the_flow_open_with_the_failure_and_finish_retries() {
    const REFUSAL: &str = "Config Document is not editable";
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Up);
    let pin = finish_with(&mut application, false);

    assert_eq!(
        refuse(&mut application, pin, REFUSAL),
        ApplicationTransition::Continue,
        "no mark is asked for"
    );
    let refused = screen(&application);
    assert!(refused.contains("Step 4 of 4"), "{refused}");
    assert!(
        refused.contains(&format!("Could not save your answer: {REFUSAL}")),
        "{refused}"
    );
    assert!(!refused.contains("Saving your answer…"), "{refused}");
    assert!(
        refused.contains("› They draw as boxes or blanks"),
        "{refused}"
    );

    // Finish tries the pin again.
    let retry = finish_with(&mut application, false);
    let retried = screen(&application);
    assert!(retried.contains("Saving your answer…"), "{retried}");
    assert!(!retried.contains("Could not save"), "{retried}");
    assert_eq!(
        refuse(&mut application, retry, REFUSAL),
        ApplicationTransition::Continue
    );

    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded,
        "Esc still leaves, without the pin"
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

#[test]
fn leaving_while_the_pin_is_saved_still_marks_the_server_onboarded() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    let pin = finish_with(&mut application, true);

    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
    assert_eq!(
        acknowledge(&mut application, pin, true),
        ApplicationTransition::Continue,
        "the pin landing late asks no second mark"
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

/// What the Server says of another edit while the pin is awaited — one the
/// flow never sent, refused late — is that edit's: the pin stays awaited, and
/// its own landing still finishes the flow.
#[test]
fn a_refusal_of_another_edit_leaves_the_pin_saving_and_its_landing_still_finishes() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    let pin = finish_with(&mut application, true);

    assert_eq!(
        refuse(
            &mut application,
            SettingRequest::new(),
            "Config Document is not editable"
        ),
        ApplicationTransition::Continue
    );
    let waiting = screen(&application);
    assert!(waiting.contains("Saving your answer…"), "{waiting}");
    assert!(!waiting.contains("Could not save"), "{waiting}");

    assert_eq!(
        acknowledge(&mut application, pin, true),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

/// A toggle the Server has not answered holds Next, so the reader never
/// passes the step on the strength of a Provider about to be turned off: its
/// row says it is turning while its tick stays as the Server says, and only
/// the toggle's own answer settles it.
#[test]
fn next_waits_on_a_toggle_the_server_has_not_answered() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_enablement(true, false, false));
    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) })
    );
    assert!(
        provider_row(&application, "Codex").contains("› [x] Codex · turning off…"),
        "{}",
        screen(&application)
    );
    assert!(screen(&application).contains("Saving your changes…"));
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application).contains("Step 1 of 4"),
        "Next waits on the toggle: {}",
        screen(&application)
    );

    // The Server's answer to another edit is not this toggle's.
    answer(&mut application, SettingRequest::new());
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 1 of 4"));
    assert!(screen(&application).contains("Saving your changes…"));

    settle_mutation(
        &mut application,
        &toggle,
        with_enablement(false, false, false),
    );
    let locked = screen(&application);
    assert!(
        locked.contains("Next is locked: enable at least one Provider."),
        "{locked}"
    );
    assert!(!locked.contains("Saving your changes…"), "{locked}");
    assert!(
        provider_row(&application, "Codex").contains("[ ] Codex · disabled"),
        "{locked}"
    );
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 1 of 4"));
}

/// A toggle the Server refuses is said beneath the rows, the row keeping the
/// tick the Server says, and Next no longer waits on it.
#[test]
fn a_refused_toggle_is_said_beneath_the_rows_and_its_row_stays_as_the_server_says() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let toggle = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        toggle,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) })
    );

    assert_eq!(
        refuse(
            &mut application,
            request_of(&toggle),
            "Config Document is not editable"
        ),
        ApplicationTransition::Continue
    );
    let refused = screen(&application);
    assert!(
        refused.contains("Could not turn Codex off: Config Document is not editable"),
        "{refused}"
    );
    assert!(!refused.contains("Saving your changes…"), "{refused}");
    assert!(
        provider_row(&application, "Codex").contains("› [x] Codex · checking"),
        "{refused}"
    );
    confirm_step(&mut application);
    assert!(
        screen(&application).contains("Step 2 of 4"),
        "{}",
        screen(&application)
    );
}

/// The Derivation step waits on the Server's word that its pin landed before
/// it moves on, holding still and saying it is saving the answer meanwhile.
/// Only the pin's own answer moves it: another edit's, even one already
/// holding the same value, or another edit's refusal, leaves it saving.
#[test]
fn the_derivation_step_waits_on_its_pin_landing_before_it_moves_on() {
    let workspace = workspace_dir();
    let settings = settings_deriving_with(DerivationErrand::FollowSession);
    let mut application = on_the_derivation_step(workspace.path(), settings.clone());
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::FollowSession));
    let saving = screen(&application);
    assert!(saving.contains("Step 2 of 4"), "{saving}");
    assert!(saving.contains("Saving your answer…"), "{saving}");

    // Its keys wait on it, save Esc.
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Left);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(screen(&application).contains("Step 2 of 4"));
    assert!(
        focused_row(&application).contains("Provider's default"),
        "{}",
        focused_row(&application)
    );

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: settings.clone(),
                pinned: vec!["derivation.errand".to_owned()],
                diagnostics: Vec::new(),
            },
        )))
        .expect("hear the Server push the pin already in force");
    answer(&mut application, SettingRequest::new());
    refuse(
        &mut application,
        SettingRequest::new(),
        "Config Document is not editable",
    );
    let waiting = screen(&application);
    assert!(waiting.contains("Step 2 of 4"), "{waiting}");
    assert!(waiting.contains("Saving your answer…"), "{waiting}");
    assert!(!waiting.contains("Could not save"), "{waiting}");

    settle_mutation(&mut application, &pin, settings);
    assert!(
        screen(&application).contains("Step 3 of 4"),
        "the pin's own answer moves the flow on: {}",
        screen(&application)
    );
}

/// A derivation pin the Server refuses keeps the flow on the Derivation
/// step, saying why, and Enter sends the answer under Row Focus again.
#[test]
fn a_refused_derivation_pin_keeps_the_step_open_with_the_failure_and_enter_retries() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    press(&mut application, KeyCode::Down);
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::Off));

    assert_eq!(
        refuse(
            &mut application,
            request_of(&pin),
            "Config Document is not editable"
        ),
        ApplicationTransition::Continue,
        "nothing moves on"
    );
    let refused = screen(&application);
    assert!(refused.contains("Step 2 of 4"), "{refused}");
    assert!(
        refused.contains("Could not save your answer: Config Document is not editable"),
        "{refused}"
    );
    assert!(!refused.contains("Saving your answer…"), "{refused}");
    assert!(focused_row(&application).contains("Off"), "{refused}");

    let retry = press(&mut application, KeyCode::Enter);
    assert_eq!(retry, derivation_pin(DerivationErrand::Off));
    let retried = screen(&application);
    assert!(retried.contains("Saving your answer…"), "{retried}");
    assert!(!retried.contains("Could not save"), "{retried}");
    settle_mutation(
        &mut application,
        &retry,
        settings_deriving_with(DerivationErrand::Off),
    );
    assert!(
        screen(&application).contains("Step 3 of 4"),
        "{}",
        screen(&application)
    );
}

#[test]
fn leaving_while_the_derivation_pin_is_saved_still_marks_the_server_onboarded() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    press(&mut application, KeyCode::Enter);
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

/// Model Options applied over the Derivation step answer it as confirming a
/// Model without any does: their pin is awaited on the step, a refusal is
/// said there and Enter opens the options again, and the pin's own landing
/// moves the flow on.
#[test]
fn options_chosen_for_the_derivation_step_wait_on_their_pin_as_confirming_does() {
    let workspace = workspace_dir();
    let mut application = choosing_options_on_the_derivation_step(workspace.path());
    let pin = apply_options(&mut application);
    assert_eq!(
        pin,
        derivation_pin(DerivationErrand::Pinned(cheap_selection(Vec::new())))
    );
    let saving = screen(&application);
    assert!(
        saving.contains("Step 2 of 4") && !saving.contains("Model Options"),
        "{saving}"
    );
    assert!(saving.contains("Saving your answer…"), "{saving}");

    refuse(
        &mut application,
        request_of(&pin),
        "Config Document is not editable",
    );
    let refused = screen(&application);
    assert!(refused.contains("Step 2 of 4"), "{refused}");
    assert!(
        refused.contains("Could not save your answer: Config Document is not editable"),
        "{refused}"
    );

    press(&mut application, KeyCode::Enter);
    assert!(
        screen(&application).contains("Model Options"),
        "Enter opens the options again: {}",
        screen(&application)
    );
    let retry = apply_options(&mut application);
    assert_eq!(
        retry,
        derivation_pin(DerivationErrand::Pinned(cheap_selection(Vec::new())))
    );
    settle_mutation(
        &mut application,
        &retry,
        settings_deriving_with(DerivationErrand::Pinned(cheap_selection(Vec::new()))),
    );
    assert!(
        screen(&application).contains("Step 3 of 4"),
        "{}",
        screen(&application)
    );
}

/// Two Providers turned off together are answered in whatever order the
/// Server's answers arrive, but the Settings in force follow the Server's own
/// stream, which never goes back: answers in reverse order leave both rows
/// off and Next locked, and a toggle the Server has answered holds Next
/// until the Settings in force show it.
#[test]
fn toggles_answered_out_of_order_leave_next_held_on_the_settings_in_force() {
    let workspace = workspace_dir();
    let mut application = attached_to_a_fresh_server(workspace.path());
    deliver_settings(&mut application, with_enablement(true, true, false));
    let codex = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        codex,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) })
    );
    press(&mut application, KeyCode::Down);
    let copilot = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        copilot,
        MutatesSetting(SettingMutation::ProviderCopilotEnabled { value: Some(false) })
    );

    // The stream pushes Codex turned off; the answer to the later toggle
    // arrives before the Settings in force show it.
    deliver_settings(&mut application, with_enablement(false, true, false));
    answer(&mut application, request_of(&copilot));
    assert!(
        provider_row(&application, "Copilot").contains("[x] Copilot · turning off…"),
        "{}",
        screen(&application)
    );
    confirm_step(&mut application);
    let held = screen(&application);
    assert!(held.contains("Step 1 of 4"), "{held}");
    assert!(held.contains("Saving your changes…"), "{held}");

    // Then the stream pushes both off, and the older toggle's answer comes
    // last of all.
    deliver_settings(&mut application, with_enablement(false, false, false));
    answer(&mut application, request_of(&codex));
    let locked = screen(&application);
    assert!(
        provider_row(&application, "Codex").contains("[ ] Codex · disabled"),
        "{locked}"
    );
    assert!(
        provider_row(&application, "Copilot").contains("[ ] Copilot · disabled"),
        "{locked}"
    );
    assert!(
        locked.contains("Next is locked: enable at least one Provider."),
        "{locked}"
    );
    assert!(!locked.contains("Saving your changes…"), "{locked}");
    confirm_step(&mut application);
    assert!(screen(&application).contains("Step 1 of 4"));
}

/// What was said of a refused toggle stands until that Provider's next
/// toggle lands, however another Provider's toggles fare meanwhile.
#[test]
fn a_refused_toggle_is_unsaid_once_that_providers_next_toggle_lands() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    let refused = press(&mut application, KeyCode::Char(' '));
    refuse(
        &mut application,
        request_of(&refused),
        "Config Document is not editable",
    );
    let refusal = "Could not turn Codex off: Config Document is not editable";
    assert!(screen(&application).contains(refusal));

    press(&mut application, KeyCode::Down);
    let copilot = press(&mut application, KeyCode::Char(' '));
    settle_mutation(
        &mut application,
        &copilot,
        with_enablement(true, false, true),
    );
    assert!(
        screen(&application).contains(refusal),
        "another Provider's toggle unsays nothing: {}",
        screen(&application)
    );

    press(&mut application, KeyCode::Up);
    let again = press(&mut application, KeyCode::Char(' '));
    assert_eq!(
        again,
        MutatesSetting(SettingMutation::ProviderCodexEnabled { value: Some(false) })
    );
    assert!(screen(&application).contains(refusal));
    answer(&mut application, request_of(&again));
    assert!(
        !screen(&application).contains(refusal),
        "{}",
        screen(&application)
    );
}

/// A retake opened before the Server's stream shows the answer just
/// finished with takes it up once it does: Row Focus the reader has not
/// touched follows Show icons as the Settings in force change.
#[test]
fn a_retake_opened_before_the_finished_answer_shows_follows_it_once_it_does() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    press(&mut application, KeyCode::Down);
    let pin = finish_with(&mut application, true);
    assert_eq!(
        answer(&mut application, pin),
        ApplicationTransition::MarkOnboarded,
        "the pin landed, though its Settings have yet to be pushed"
    );

    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    assert!(screen(&application).contains("Step 1 of 4"));
    deliver_settings(&mut application, with_show_icons(true));
    on_the_icons_step_holding(&mut application, with_show_icons(true));
    let shown = screen(&application);
    assert!(shown.contains("› They draw as icons"), "{shown}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        pins_show_icons(true),
        "Finish pins the answer in force rather than the one the retake opened on"
    );
}

/// Row Focus the reader moved on a retake is theirs: a snapshot the Server
/// pushes later does not move it.
#[test]
fn a_retake_row_focus_the_reader_moved_stays_where_they_moved_it() {
    let workspace = workspace_dir();
    let mut application = retaking_the_icons_step(workspace.path(), with_show_icons(false));
    assert!(screen(&application).contains("› They draw as boxes or blanks"));
    press(&mut application, KeyCode::Up);
    assert!(screen(&application).contains("› They draw as icons"));

    deliver_settings(&mut application, with_show_icons(false));
    let shown = screen(&application);
    assert!(shown.contains("› They draw as icons"), "{shown}");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        pins_show_icons(true)
    );
}

/// A first run selects nothing, whatever the Settings in force come to say
/// while the reader looks.
#[test]
fn a_first_run_selects_nothing_whatever_snapshots_arrive_meanwhile() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    on_the_icons_step(&mut application);
    deliver_settings(&mut application, with_show_icons(true));
    let shown = screen(&application);
    assert!(!shown.contains("› They draw"), "{shown}");
}

/// The Derivation step's Row Focus, untouched, follows the value the Setting
/// holds as the Settings in force change; once the reader moves it, or
/// chooses where it stands, it is theirs.
#[test]
fn the_derivation_steps_untouched_row_focus_follows_the_setting_in_force() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    assert!(focused_row(&application).contains("Provider's default"));
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::Off),
    );
    assert!(
        focused_row(&application).contains("Off"),
        "{}",
        focused_row(&application)
    );

    press(&mut application, KeyCode::Down);
    assert!(
        focused_row(&application).contains("GPT-5 ·"),
        "{}",
        focused_row(&application)
    );
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    assert!(
        focused_row(&application).contains("GPT-5 ·"),
        "Row Focus the reader moved stays: {}",
        focused_row(&application)
    );
}

/// A row the reader chose without moving to it is theirs as well: a
/// snapshot another Client caused, arriving while the pin is saved, leaves
/// Row Focus on it.
#[test]
fn the_derivation_row_the_reader_chose_stays_focused_while_its_pin_is_saved() {
    let workspace = workspace_dir();
    let mut application = on_the_derivation_step(
        workspace.path(),
        settings_deriving_with(DerivationErrand::FollowSession),
    );
    let pin = press(&mut application, KeyCode::Enter);
    assert_eq!(pin, derivation_pin(DerivationErrand::FollowSession));
    deliver_settings(
        &mut application,
        settings_deriving_with(DerivationErrand::Off),
    );
    assert!(
        focused_row(&application).contains("Provider's default"),
        "{}",
        focused_row(&application)
    );
}

/// Workspace Roots rooted the way the platform under test roots an absolute
/// path: a POSIX path is relative to Windows, which is given drive-rooted
/// ones instead.
#[cfg(windows)]
const WORK_ROOT: &str = r"D:\work";
#[cfg(not(windows))]
const WORK_ROOT: &str = "/srv/work";
#[cfg(windows)]
const ARCHIVE_ROOT: &str = r"E:\archive";
#[cfg(not(windows))]
const ARCHIVE_ROOT: &str = "/mnt/archive";

/// Effective settings naming `roots` as the Workspace Roots.
fn with_workspace_roots(roots: &[&str]) -> EffectiveSettings {
    EffectiveSettings {
        workspace: WorkspaceSettings {
            roots: roots.iter().map(|root| (*root).to_owned()).collect(),
        },
        ..EffectiveSettings::default()
    }
}

/// The pin Next sends on the Workspace Roots step for a draft of `roots`.
fn pins_roots(roots: &[&str]) -> MutatesSetting {
    MutatesSetting(SettingMutation::WorkspaceRoots {
        value: Some(roots.iter().map(|root| (*root).to_owned()).collect()),
    })
}

/// A fresh Server's Client holding `settings`, with the flow walked on to
/// its Workspace Roots step.
fn on_the_roots_step(workspace: &Path, settings: EffectiveSettings) -> Application {
    let mut application = attached_to_a_fresh_server(workspace);
    deliver_settings(&mut application, settings.clone());
    confirm_step(&mut application);
    pass_the_derivation_step(&mut application, settings);
    let shown = screen(&application);
    assert!(shown.contains("Step 3 of 4 · Workspace Roots"), "{shown}");
    application
}

/// The Directory Browser's ask of the Client's own Server for the children
/// of the directory it is rooted at.
fn expect_listing(transition: ApplicationTransition) -> (DirectoryListingId, PathBuf) {
    let ApplicationTransition::ListDirectory {
        outlook,
        listing_id,
        request,
    } = transition
    else {
        panic!("the browser asks the Server for a directory's children, not {transition:?}");
    };
    assert_eq!(
        outlook,
        Outlook::Local,
        "the Server whose Roots are named is the one browsed"
    );
    (listing_id, request.path)
}

/// The Server's listing of `root` with plain child directories named
/// `children`.
fn answer_listing(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    children: &[&str],
) {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(DirectoryListing {
                root: root.to_owned(),
                parent: root.parent().map(Path::to_owned),
                source_control: DirectorySourceControl::Plain,
                children: children
                    .iter()
                    .map(|name| ChildDirectory {
                        name: (*name).to_owned(),
                        path: root.join(name),
                        source_control: DirectorySourceControl::Plain,
                        hidden: false,
                    })
                    .collect(),
            }),
        })
        .expect("deliver the Server's listing");
}

#[test]
fn the_roots_step_says_what_roots_are_for_and_teaches_its_keys() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    let shown = screen(&application);
    assert!(shown.contains("Directories your work lives in."), "{shown}");
    assert!(shown.contains("› add a Root"), "{shown}");
    assert!(shown.contains("None is fine"), "{shown}");
    assert!(shown.contains("  Confirm"), "{shown}");
    let wide = rendered_application_rows_at(&application, 140, 24).join("\n");
    assert!(
        wide.contains("Ctrl+D Remove · ↑↓ Move · Enter Browse · ← Back · Esc Leave"),
        "{wide}"
    );

    focus_confirm(&mut application);
    let wide = rendered_application_rows_at(&application, 140, 24).join("\n");
    assert!(
        wide.contains("Ctrl+D Remove · ↑↓ Move · Enter Next · ← Back · Esc Leave"),
        "from the Confirm row Enter goes on: {wide}"
    );
}

/// Confirm pins the whole draft as one list and waits on the Server's word
/// that it landed, holding the draft still meanwhile, before the flow goes
/// on.
#[test]
fn roots_typed_on_the_step_are_pinned_as_one_list_once_next_is_pressed() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/code");
    press(&mut application, KeyCode::Down);
    type_terminal_text(&mut application, WORK_ROOT);
    let drafted = screen(&application);
    assert!(drafted.contains("~/code"), "{drafted}");
    assert!(drafted.contains(&format!("› {WORK_ROOT}")), "{drafted}");

    let pin = confirm_step(&mut application);
    assert_eq!(pin, pins_roots(&["~/code", WORK_ROOT]));
    let saving = screen(&application);
    assert!(saving.contains("Step 3 of 4"), "{saving}");
    assert!(saving.contains("Saving your answer…"), "{saving}");

    // Moves, edits, Next and Back all wait on it.
    press(&mut application, KeyCode::Up);
    type_terminal_text(&mut application, "x");
    press_with(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    press(&mut application, KeyCode::Left);
    let waiting = screen(&application);
    assert!(waiting.contains("Step 3 of 4"), "{waiting}");
    assert!(waiting.contains("› Confirm"), "{waiting}");
    assert!(waiting.contains(WORK_ROOT), "{waiting}");
    assert!(!waiting.contains(&format!("{WORK_ROOT}x")), "{waiting}");

    settle_mutation(
        &mut application,
        &pin,
        with_workspace_roots(&["~/code", WORK_ROOT]),
    );
    let moved_on = screen(&application);
    assert!(moved_on.contains("Step 4 of 4 · Icons"), "{moved_on}");
}

/// No Roots is an answer as good as any, and Roots left as they stand are
/// not pinned again.
#[test]
fn the_roots_in_force_fill_the_step_and_pass_unpinned_until_edited() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), with_workspace_roots(&[WORK_ROOT]));
    let shown = screen(&application);
    assert!(shown.contains(&format!("› {WORK_ROOT}")), "{shown}");

    // A draft no one has edited follows the Roots in force.
    deliver_settings(
        &mut application,
        with_workspace_roots(&[WORK_ROOT, ARCHIVE_ROOT]),
    );
    let followed = screen(&application);
    assert!(followed.contains(ARCHIVE_ROOT), "{followed}");

    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue,
        "Roots left as they stand pin nothing"
    );
    assert!(screen(&application).contains("Step 4 of 4 · Icons"));

    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    focus_confirm(&mut application);
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue,
        "no Roots passes the step, Space on the Confirm row passing it as Enter does"
    );
    assert!(screen(&application).contains("Step 4 of 4 · Icons"));
}

/// Once the reader has edited the draft it is theirs, and the Roots in force
/// changing elsewhere leaves it as they left it.
#[test]
fn an_edited_roots_draft_follows_the_roots_in_force_no_more() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), with_workspace_roots(&[WORK_ROOT]));
    press_with(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL);
    deliver_settings(
        &mut application,
        with_workspace_roots(&[WORK_ROOT, ARCHIVE_ROOT]),
    );
    let shown = screen(&application);
    assert!(
        !shown.contains(WORK_ROOT) && !shown.contains(ARCHIVE_ROOT),
        "{shown}"
    );
    assert_eq!(confirm_step(&mut application), pins_roots(&[]));
}

#[test]
fn a_root_neither_absolute_nor_beginning_with_a_tilde_holds_the_step_naming_it() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "projects");
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    let refused = screen(&application);
    assert!(refused.contains("Step 3 of 4"), "{refused}");
    assert!(
        refused.contains("› projects"),
        "focus moves from the Confirm row to the entry refused: {refused}"
    );
    assert!(
        refused.contains("\"projects\" is not an absolute path or a path beginning with ~"),
        "{refused}"
    );

    for _ in "projects".chars() {
        press(&mut application, KeyCode::Backspace);
    }
    type_terminal_text(&mut application, "~/projects");
    assert!(!screen(&application).contains("is not an absolute path"));
    assert_eq!(confirm_step(&mut application), pins_roots(&["~/projects"]));
}

/// Back keeps the draft as left without saving it, and leaving the flow
/// drops it unsaved.
#[test]
fn going_back_keeps_the_roots_draft_unsaved_and_leaving_drops_it() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/code");
    assert_eq!(
        press(&mut application, KeyCode::Left),
        ApplicationTransition::Continue,
        "Back saves nothing"
    );
    assert!(screen(&application).contains("Step 2 of 4"));

    pass_the_derivation_step(&mut application, EffectiveSettings::default());
    let shown = screen(&application);
    assert!(shown.contains("› ~/code"), "{shown}");
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::MarkOnboarded
    );
    assert!(!onboarding_shown(&application), "{}", screen(&application));
}

#[test]
fn ctrl_d_removes_the_focused_root_and_a_paste_lands_in_the_focused_row() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(
        workspace.path(),
        with_workspace_roots(&[WORK_ROOT, ARCHIVE_ROOT]),
    );
    press_with(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut application, KeyCode::Down);
    assert!(screen(&application).contains("› add a Root"));
    application
        .handle_terminal_event(InputEvent::Paste("~/code\n".to_owned()))
        .expect("paste a Root");
    assert_eq!(
        confirm_step(&mut application),
        pins_roots(&[ARCHIVE_ROOT, "~/code"])
    );
}

/// Each edit of the draft is a semantic command, so a plugin or the pointer
/// reaches it as the keys do, and only while the step is on show.
#[test]
fn roots_step_commands_can_be_invoked_semantically() {
    let workspace = workspace_dir();
    let mut application = offered_onboarding(workspace.path());
    invoke(
        &mut application,
        CommandId::InvokeSemanticText(
            SemanticCommandId::OnboardingRootsInsert,
            "~/early".to_owned(),
        ),
    );
    confirm_step(&mut application);
    pass_the_derivation_step(&mut application, EffectiveSettings::default());
    assert!(!screen(&application).contains("~/early"));

    for command in [
        CommandId::InvokeSemanticText(
            SemanticCommandId::OnboardingRootsInsert,
            "~/code".to_owned(),
        ),
        CommandId::InvokeSemantic(SemanticCommandId::OnboardingRootsDeleteBackward),
        CommandId::InvokeSemantic(SemanticCommandId::OnboardingRootsNext),
        CommandId::InvokeSemanticText(SemanticCommandId::OnboardingRootsInsert, "~/x".to_owned()),
        CommandId::InvokeSemantic(SemanticCommandId::OnboardingRootsRemove),
        CommandId::InvokeSemantic(SemanticCommandId::OnboardingRootsPrevious),
    ] {
        assert_eq!(
            invoke(&mut application, command),
            ApplicationTransition::Continue
        );
    }
    assert!(
        screen(&application).contains("› ~/cod"),
        "{}",
        screen(&application)
    );
    assert_eq!(confirm_step(&mut application), pins_roots(&["~/cod"]));
}

fn invoke(application: &mut Application, command: CommandId) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(command))
        .expect("invoke an Onboarding command")
}

/// Space or Enter on the row a Root is added at browses the Client's own
/// Server's directories over the step, rooted where the Landing works; a
/// choice is an entry of the draft, Enter on it browses again, and Esc goes
/// back to the step rather than leaving the flow.
#[test]
fn space_or_enter_on_the_add_row_browses_for_a_root_over_the_flow() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    assert!(screen(&application).contains("› add a Root"));
    let (listing_id, root) = expect_listing(press(&mut application, KeyCode::Char(' ')));
    assert_eq!(root, workspace.path());
    answer_listing(&mut application, listing_id, &root, &["projects", "notes"]);
    let browsing = screen(&application);
    assert!(
        browsing.contains("Path:") && browsing.contains("projects"),
        "the browser stands over the flow: {browsing}"
    );

    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "a Root chosen is a draft, and no Workspace is resolved"
    );
    let chosen = screen(&application);
    let projects = root.join("projects").display().to_string();
    assert!(
        !chosen.contains("Path:") && chosen.contains("Step 3 of 4"),
        "{chosen}"
    );
    assert!(chosen.contains("projects"), "{chosen}");

    let (listing_id, root) = expect_listing(press(&mut application, KeyCode::Enter));
    answer_listing(&mut application, listing_id, &root, &["projects", "notes"]);
    type_terminal_text(&mut application, "x");
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue,
        "Esc backs out of the browser alone"
    );
    let back = screen(&application);
    assert!(
        !back.contains("Path:") && back.contains("Step 3 of 4"),
        "{back}"
    );
    assert_eq!(
        confirm_step(&mut application),
        pins_roots(&[&projects]),
        "nothing typed while the browser held the keys reached the draft"
    );

    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    expect_listing(press(&mut application, KeyCode::Enter));
    assert!(
        screen(&application).contains("Path:"),
        "Enter on the row a Root is added at browses as Space does: {}",
        screen(&application)
    );
}

/// Enter on a Root browses for a directory to put in its place.
#[test]
fn enter_on_a_root_browses_for_one_to_take_its_place() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), with_workspace_roots(&[WORK_ROOT]));
    assert!(screen(&application).contains(&format!("› {WORK_ROOT}")));
    let (listing_id, root) = expect_listing(press(&mut application, KeyCode::Enter));
    answer_listing(&mut application, listing_id, &root, &["projects"]);
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);
    let projects = root.join("projects").display().to_string();
    assert_eq!(confirm_step(&mut application), pins_roots(&[&projects]));
}

/// Space on a Root is part of what it says, as any other character typed
/// there is, rather than a way to browse.
#[test]
fn space_on_a_root_types_a_space() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/my");
    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue
    );
    type_terminal_text(&mut application, "code");
    let shown = screen(&application);
    assert!(shown.contains("› ~/my code"), "{shown}");
    assert!(!shown.contains("Path:"), "{shown}");
    assert_eq!(confirm_step(&mut application), pins_roots(&["~/my code"]));
}

/// Tab is no way to browse: Enter and Space are.
#[test]
fn tab_on_the_roots_step_browses_for_nothing() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    assert_eq!(
        press(&mut application, KeyCode::Tab),
        ApplicationTransition::Continue
    );
    let shown = screen(&application);
    assert!(!shown.contains("Path:"), "{shown}");
    assert!(shown.contains("Step 3 of 4"), "{shown}");
}

#[test]
fn the_browser_onboarding_opened_closes_with_it_when_another_client_takes_it() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    let (listing_id, root) = expect_listing(press(&mut application, KeyCode::Enter));
    answer_listing(&mut application, listing_id, &root, &["projects"]);

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Onboarded(true)))
        .expect("hear that another Client onboarded the Server");
    let after = screen(&application);
    assert!(
        !onboarding_shown(&application) && !after.contains("Path:"),
        "nothing the flow offered stands to be chosen: {after}"
    );
}

/// A pin the Server refuses keeps the flow on the step, saying why beneath
/// the rows, and Enter on the Confirm row sends it again.
#[test]
fn a_refused_roots_pin_keeps_the_step_open_with_the_failure_and_next_retries() {
    const REFUSAL: &str = "Config Document is not editable";
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/code");
    let pin = confirm_step(&mut application);
    assert_eq!(pin, pins_roots(&["~/code"]));
    assert_eq!(
        refuse(&mut application, request_of(&pin), REFUSAL),
        ApplicationTransition::Continue
    );
    let refused = screen(&application);
    assert!(refused.contains("Step 3 of 4"), "{refused}");
    assert!(
        refused.contains(&format!("Could not save your answer: {REFUSAL}")),
        "{refused}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        pins_roots(&["~/code"])
    );
}

/// A screen too short for all the step says keeps the focused row, the
/// Confirm row, and the keys at the foot, giving way the rest.
#[test]
fn a_short_screen_keeps_the_focused_root_and_the_keys() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(
        workspace.path(),
        with_workspace_roots(&["~/a", "~/b", "~/c", "~/d", "~/e", "~/f"]),
    );
    for _ in 0..4 {
        press(&mut application, KeyCode::Down);
    }
    for height in [9, 12, 16] {
        let shown = rendered_application_rows_at(&application, 80, height).join("\n");
        assert!(shown.contains("Step 3 of 4"), "{height}: {shown}");
        assert!(shown.contains("› ~/e"), "{height}: {shown}");
        assert!(shown.contains("  Confirm"), "{height}: {shown}");
        assert!(shown.contains("Enter Browse"), "{height}: {shown}");
    }
}

/// Browsing finds nothing while the draft's pin is saved, since a choice
/// would land on a draft held still.
#[test]
fn browsing_finds_nothing_while_the_roots_pin_is_saved() {
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/code");
    assert_eq!(confirm_step(&mut application), pins_roots(&["~/code"]));
    assert_eq!(
        invoke(
            &mut application,
            CommandId::InvokeSemantic(SemanticCommandId::OnboardingRootsBrowse)
        ),
        ApplicationTransition::Continue
    );
    assert!(!screen(&application).contains("Path:"));
}

/// A refusal is said only until the step is passed: a draft taken back to
/// the Roots in force passes unpinned, leaving no failure to find on return.
#[test]
fn passing_the_roots_step_unpinned_leaves_no_earlier_refusal_standing() {
    const REFUSAL: &str = "Config Document is not editable";
    let workspace = workspace_dir();
    let mut application = on_the_roots_step(workspace.path(), EffectiveSettings::default());
    type_terminal_text(&mut application, "~/code");
    let pin = confirm_step(&mut application);
    refuse(&mut application, request_of(&pin), REFUSAL);
    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Up);
    press_with(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
    assert!(screen(&application).contains("Step 4 of 4"));
    press(&mut application, KeyCode::Left);
    let back = screen(&application);
    assert!(!back.contains("Could not save"), "{back}");
}

/// Taking the flow again opens the step on the Roots in force.
#[test]
fn taking_onboarding_again_opens_the_roots_step_on_the_roots_in_force() {
    let workspace = workspace_dir();
    let settings = with_workspace_roots(&[WORK_ROOT]);
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, settings.clone());
    type_terminal_text(&mut application, "/onboarding");
    press(&mut application, KeyCode::Enter);
    confirm_step(&mut application);
    pass_the_derivation_step(&mut application, settings);
    let shown = screen(&application);
    assert!(shown.contains("Step 3 of 4 · Workspace Roots"), "{shown}");
    assert!(shown.contains(&format!("› {WORK_ROOT}")), "{shown}");
    assert_eq!(
        confirm_step(&mut application),
        ApplicationTransition::Continue
    );
}
