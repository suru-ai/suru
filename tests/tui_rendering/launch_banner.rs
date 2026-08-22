//! The launch view's banner for configuration problems found at startup.

use std::path::{Path, PathBuf};

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::support::{
    connected_application, rendered_application_rows, rendered_application_rows_at,
    type_terminal_text,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, SettingsDiagnostic, SettingsDiagnosticSeverity, SettingsSnapshot,
    },
    tui::{Application, ApplicationEvent},
};

/// A client that connected and then received the effective-settings snapshot,
/// which is the order the protocol guarantees: the snapshot leads the
/// lifecycle stream, so the launch view has the startup diagnostics in hand
/// before the user can touch anything.
fn launched_with(workspace: &Path, diagnostics: Vec<SettingsDiagnostic>) -> Application {
    let mut application = connected_application(workspace);
    deliver_snapshot(&mut application, diagnostics);
    application
}

fn deliver_snapshot(application: &mut Application, diagnostics: Vec<SettingsDiagnostic>) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings::default(),
                pinned: Vec::new(),
                diagnostics,
            },
        )))
        .expect("receive the effective-settings snapshot");
}

/// The launch view's top row, as the reader sees it: the banner is inset from
/// the terminal edge exactly as the launch view's other rows are.
fn banner_row(rows: &[String]) -> String {
    rows[0].trim().to_owned()
}

fn config_file(name: &str) -> PathBuf {
    PathBuf::from("/home/user/.config/suru").join(name)
}

fn unreadable_document() -> SettingsDiagnostic {
    SettingsDiagnostic {
        severity: SettingsDiagnosticSeverity::Error,
        file: config_file("suru.jsonc"),
        key: None,
        message: "ignored because it is not valid JSONC: Unexpected token on line 3 column 5"
            .to_owned(),
    }
}

fn unknown_key() -> SettingsDiagnostic {
    SettingsDiagnostic {
        severity: SettingsDiagnosticSeverity::Warning,
        file: config_file("suru.jsonc"),
        key: Some("transcript.defaultFoldPostures".to_owned()),
        message: "ignored because it is not a known Setting".to_owned(),
    }
}

fn mistyped_key() -> SettingsDiagnostic {
    SettingsDiagnostic {
        severity: SettingsDiagnosticSeverity::Warning,
        file: config_file("suru.jsonc"),
        key: Some("provider.codex.reasoningSummary".to_owned()),
        message: "ignored because its value is not one of \"auto\", \"concise\", \"detailed\", or \"none\""
            .to_owned(),
    }
}

#[test]
fn a_syntax_broken_config_document_banners_the_failure_and_points_at_the_log() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(workspace.path(), vec![unreadable_document()]);

    let launch = rendered_application_rows(&application);
    let banner = banner_row(&launch);
    assert!(
        banner.starts_with("× "),
        "a whole Config Document being ignored banners at error severity: {banner:?}"
    );
    assert!(
        banner.contains("suru.jsonc") && banner.contains("not valid JSONC"),
        "the banner names the file and why it was ignored: {banner:?}"
    );
    assert!(
        banner.contains("see the Log"),
        "the banner points at the Log, where the whole diagnostic is: {banner:?}"
    );
    assert!(
        launch
            .join("\n")
            .contains("What would you like to work on?"),
        "the banner sits above the launch view rather than replacing it: {launch:?}"
    );
}

#[test]
fn an_ignored_duplicate_config_document_banners_the_file_it_dropped() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(
        workspace.path(),
        vec![SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: config_file("suru.json"),
            key: None,
            message: "ignored because suru.jsonc exists and wins".to_owned(),
        }],
    );

    let banner = banner_row(&rendered_application_rows(&application));
    assert!(
        banner.starts_with("! "),
        "an ignored duplicate is a warning, not a failure: {banner:?}"
    );
    assert!(
        banner.contains("suru.json ignored because suru.jsonc exists and wins"),
        "the banner names the dropped file and what beat it: {banner:?}"
    );
}

#[test]
fn per_key_ignores_are_counted_in_the_banner_summary() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(workspace.path(), vec![unknown_key(), mistyped_key()]);

    let banner = banner_row(&rendered_application_rows(&application));
    assert!(
        banner.starts_with("! "),
        "keys ignored one at a time are a warning: {banner:?}"
    );
    assert!(
        banner.contains("2 keys ignored in suru.jsonc"),
        "the banner counts the ignored keys and names their file: {banner:?}"
    );
    assert!(
        banner.contains("see the Log"),
        "the Log is where the key paths and reasons are: {banner:?}"
    );
}

#[test]
fn a_file_level_failure_is_worded_ahead_of_the_keys_it_shares_the_banner_with() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(
        workspace.path(),
        vec![unknown_key(), unreadable_document(), mistyped_key()],
    );

    let banner = banner_row(&rendered_application_rows_at(&application, 160, 15));
    let failure = banner
        .find("not valid JSONC")
        .unwrap_or_else(|| panic!("banner omitted the file-level failure: {banner:?}"));
    let keys = banner
        .find("2 keys ignored")
        .unwrap_or_else(|| panic!("banner omitted the per-key count: {banner:?}"));
    assert!(
        failure < keys,
        "the file-level failure is worded loudest, ahead of the key count: {banner:?}"
    );
    assert!(
        banner.starts_with("× "),
        "one error among warnings banners at error severity: {banner:?}"
    );
}

#[test]
fn an_ignored_document_is_worded_ahead_of_a_document_merely_dropped() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(
        workspace.path(),
        vec![
            SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Warning,
                file: config_file("suru.json"),
                key: None,
                message: "ignored because suru.jsonc exists and wins".to_owned(),
            },
            unreadable_document(),
        ],
    );

    let banner = banner_row(&rendered_application_rows_at(&application, 160, 15));
    let failure = banner
        .find("not valid JSONC")
        .unwrap_or_else(|| panic!("banner omitted the failure: {banner:?}"));
    let dropped = banner
        .find("exists and wins")
        .unwrap_or_else(|| panic!("banner omitted the dropped duplicate: {banner:?}"));
    assert!(
        failure < dropped,
        "the loudest failure leads, whatever order the loader found them in: {banner:?}"
    );
}

#[test]
fn a_summary_too_long_for_the_terminal_gives_way_before_the_pointer_at_the_log_does() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(
        workspace.path(),
        vec![unreadable_document(), unknown_key(), mistyped_key()],
    );

    for width in [40, 60, 80] {
        let banner = banner_row(&rendered_application_rows_at(&application, width, 15));
        assert!(
            banner.starts_with("× ") && banner.ends_with("see the Log"),
            "at width {width} the banner still tells the reader where to look: {banner:?}"
        );
    }
}

#[test]
fn an_interaction_the_launch_view_makes_nothing_of_still_dismisses_the_banner() {
    let unbound_key = InputEvent::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
    let click = InputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 4,
        modifiers: KeyModifiers::NONE,
    });
    for interaction in [unbound_key, click] {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut application = launched_with(workspace.path(), vec![unreadable_document()]);

        application
            .handle_terminal_event(interaction.clone())
            .expect("deliver an interaction the launch view maps to no command");

        let dismissed = rendered_application_rows(&application).join("\n");
        assert!(
            !dismissed.contains("see the Log"),
            "{interaction:?} is an interaction even where it commands nothing: {dismissed}"
        );
    }
}

#[test]
fn the_terminal_resizing_around_the_reader_leaves_the_banner_standing() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = launched_with(workspace.path(), vec![unreadable_document()]);

    application
        .handle_terminal_event(InputEvent::Resize(100, 30))
        .expect("deliver a resize");

    let launch = rendered_application_rows_at(&application, 100, 30).join("\n");
    assert!(
        launch.contains("see the Log"),
        "a resize is the terminal's doing, not the reader's: {launch}"
    );
}

#[test]
fn a_clean_configuration_renders_no_banner() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = launched_with(workspace.path(), Vec::new());

    let launch = rendered_application_rows(&application);
    assert!(
        !launch.join("\n").contains("see the Log"),
        "a startup with nothing to report says nothing: {launch:?}"
    );
    assert_eq!(
        launch,
        rendered_application_rows(&connected_application(workspace.path())),
        "a clean snapshot leaves the launch view exactly as it was"
    );
}

#[test]
fn the_next_interaction_dismisses_the_banner_for_the_rest_of_the_run() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = launched_with(workspace.path(), vec![unreadable_document()]);
    assert!(
        banner_row(&rendered_application_rows(&application)).contains("see the Log"),
        "the banner shows before the user has touched anything"
    );

    type_terminal_text(&mut application, "hello");

    let dismissed = rendered_application_rows(&application).join("\n");
    assert!(
        !dismissed.contains("see the Log"),
        "the reader's next interaction dismisses the banner: {dismissed}"
    );
    assert!(
        dismissed.contains("hello"),
        "the banner never blocks input — the keystroke that dismissed it still typed: {dismissed}"
    );

    deliver_snapshot(&mut application, vec![unreadable_document()]);
    let reconnected = rendered_application_rows(&application).join("\n");
    assert!(
        !reconnected.contains("see the Log"),
        "a banner the reader dismissed does not come back during the run: {reconnected}"
    );
}
