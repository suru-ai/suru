//! The settings panel: every defined Setting, and editing one from the TUI.

use std::path::Path;

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};

use crate::support::{connected_application, rendered_application_rows, type_terminal_text};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, FoldPosture, ReasoningSummaryDetail, SettingMutation, SettingsSnapshot,
        TranscriptSettings,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

/// A connected client holding the effective settings the server pushed, which
/// is the only place the panel reads a value from.
fn client_showing(workspace: &Path, settings: EffectiveSettings, pinned: &[&str]) -> Application {
    let mut application = connected_application(workspace);
    deliver_snapshot(&mut application, settings, pinned);
    application
}

fn deliver_snapshot(application: &mut Application, settings: EffectiveSettings, pinned: &[&str]) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            snapshot(settings, pinned),
        )))
        .expect("receive the effective-settings snapshot");
}

/// Effective settings whose only departure from the built-in defaults is the
/// Fold posture a Session view opens at.
fn opening_at(posture: FoldPosture) -> EffectiveSettings {
    EffectiveSettings {
        transcript: TranscriptSettings {
            default_fold_posture: posture,
        },
        ..EffectiveSettings::default()
    }
}

fn snapshot(settings: EffectiveSettings, pinned: &[&str]) -> SettingsSnapshot {
    SettingsSnapshot {
        settings,
        pinned: pinned.iter().map(|key| (*key).to_owned()).collect(),
        diagnostics: Vec::new(),
    }
}

fn press(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("handle a settings panel key")
}

fn open_panel(application: &mut Application) {
    press(application, KeyCode::Char('x'), KeyModifiers::CONTROL);
    press(application, KeyCode::Char(','), KeyModifiers::NONE);
}

/// The panel row for one Setting, as the reader sees it.
fn row(application: &Application, label: &str) -> String {
    let rows = rendered_application_rows(application);
    rows.iter()
        .find(|row| row.contains(label))
        .unwrap_or_else(|| panic!("the settings panel showed no row for {label:?}: {rows:?}"))
        .trim()
        .to_owned()
}

#[test]
fn the_leader_key_and_the_slash_command_both_open_the_panel_and_escape_closes_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);

    open_panel(&mut application);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Settings"),
        "leader+, opens the settings panel"
    );

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    let closed = rendered_application_rows(&application).join("\n");
    assert!(
        !closed.contains("Default Fold posture"),
        "Esc closes the panel: {closed}"
    );

    type_terminal_text(&mut application, "/settings");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Default Fold posture"),
        "the /settings slash entry opens the same panel"
    );
}

#[test]
fn every_defined_setting_shows_its_effective_value_and_whether_a_config_document_pins_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(
        workspace.path(),
        opening_at(FoldPosture::Expanded),
        &["transcript.defaultFoldPosture"],
    );
    open_panel(&mut application);

    let fold = row(&application, "Default Fold posture");
    assert!(
        fold.contains("expanded") && fold.contains("[pinned]"),
        "a Setting a Config Document pins shows its pinned value: {fold:?}"
    );
    let reasoning = row(&application, "Codex Reasoning summary");
    assert!(
        reasoning.contains("auto") && reasoning.contains("[default]"),
        "a Setting nothing pins rides its built-in default: {reasoning:?}"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("transcript.defaultFoldPosture"),
        "the focused Setting names the key a Config Document would spell"
    );
}

#[test]
fn choosing_a_value_pins_it_and_the_row_follows_the_refreshed_snapshot() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    let transition = press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        transition,
        ApplicationTransition::MutateSetting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        }),
        "choosing a value emits its set mutation immediately"
    );
    assert!(
        row(&application, "Default Fold posture").contains("folded [default]"),
        "the row waits for the server rather than showing an edit the file has not taken"
    );

    application
        .handle_event(ApplicationEvent::SettingMutated(snapshot(
            opening_at(FoldPosture::Expanded),
            &["transcript.defaultFoldPosture"],
        )))
        .expect("receive the settings the edit left in force");
    assert!(
        row(&application, "Default Fold posture").contains("expanded [pinned]"),
        "the row reflects the refreshed snapshot the edit produced"
    );
}

#[test]
fn a_setting_cycles_through_every_value_it_offers_in_both_directions() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);

    assert_eq!(
        press(&mut application, KeyCode::Right, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Concise),
        }),
        "the next value follows the one in force"
    );
    assert_eq!(
        press(&mut application, KeyCode::Left, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::None),
        }),
        "stepping back from the first value wraps to the last"
    );
}

#[test]
fn resetting_a_pinned_setting_unpins_it_and_the_row_returns_to_the_default() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(
        workspace.path(),
        opening_at(FoldPosture::Expanded),
        &["transcript.defaultFoldPosture"],
    );
    open_panel(&mut application);

    assert_eq!(
        press(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::TranscriptDefaultFoldPosture {
            value: None,
        }),
        "the reset action emits the unset mutation"
    );
    application
        .handle_event(ApplicationEvent::SettingMutated(snapshot(
            EffectiveSettings::default(),
            &[],
        )))
        .expect("receive the settings the reset left in force");
    assert!(
        row(&application, "Default Fold posture").contains("folded [default]"),
        "the row returns to the built-in default, unmarked as pinned"
    );
}

#[test]
fn resetting_a_setting_unpins_it_whether_or_not_the_panel_thinks_it_is_pinned() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    assert_eq!(
        press(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::TranscriptDefaultFoldPosture {
            value: None,
        }),
        "the reset always asks the server to unpin, because the file is the server's to know"
    );
}

#[test]
fn an_edit_the_server_refuses_says_so_and_leaves_the_row_where_it_was() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    application
        .handle_event(ApplicationEvent::SettingMutationFailed(
            "Config Document \"suru.jsonc\" is not valid JSONC".to_owned(),
        ))
        .expect("hear that the edit never landed");
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("not valid JSONC"),
        "the panel says why the Config Document did not change: {panel}"
    );
    assert!(
        row(&application, "Default Fold posture").contains("folded [default]"),
        "a refused edit leaves the Setting exactly where it was"
    );

    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("not valid JSONC"),
        "the complaint does not outlive the attempt that earned it"
    );
}

/// The panel is driven by semantic command IDs so other surfaces can invoke
/// the same behaviors, which means an edit command can arrive from somewhere
/// the panel is not. None of them may reach a Setting the reader never opened.
#[test]
fn an_edit_command_invoked_while_the_panel_is_closed_touches_no_setting() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);

    for command in [
        SemanticCommandId::SettingsValueNext,
        SemanticCommandId::SettingsValuePrevious,
        SemanticCommandId::SettingsReset,
    ] {
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    command
                )))
                .expect("invoke a settings command with the panel closed"),
            ApplicationTransition::Continue,
            "{command:?} edited a Setting with no panel open"
        );
    }
}

#[test]
fn the_open_panel_takes_the_keys_the_composer_would_otherwise_get() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    type_terminal_text(&mut application, "not a Prompt");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("not a Prompt"),
        "typing over an open panel never reaches the composer"
    );

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SettingsClose,
            )))
            .expect("close a panel that is already closed"),
        ApplicationTransition::Continue,
        "closing a closed panel changes nothing"
    );
}
