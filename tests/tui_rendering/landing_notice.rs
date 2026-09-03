//! The Landing's Notice for configuration problems found at startup.

use std::path::{Path, PathBuf};

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::support::{
    connected_application, rendered_application_rows, rendered_application_rows_at,
    type_terminal_text, workspace_dir,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, SettingsDiagnostic, SettingsDiagnosticSeverity, SettingsSnapshot,
        SidebarSettings, SidebarVisibility,
    },
    tui::{Application, ApplicationEvent},
};

/// A client that connected and then received the effective-settings snapshot,
/// which is the order the protocol guarantees: the snapshot leads the
/// lifecycle stream, so the Landing has the startup diagnostics in hand before
/// the user can touch anything.
fn landing_showing(workspace: &Path, diagnostics: Vec<SettingsDiagnostic>) -> Application {
    let mut application = connected_application(workspace);
    deliver_snapshot(&mut application, diagnostics);
    application
}

fn deliver_snapshot(application: &mut Application, diagnostics: Vec<SettingsDiagnostic>) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    // The Notice is the Landing's own row, so the Sidebar stays
                    // off the frame rather than sharing it.
                    sidebar: SidebarSettings {
                        initial_visibility: SidebarVisibility::Hidden,
                        ..SidebarSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics,
            },
        )))
        .expect("receive the effective-settings snapshot");
}

/// The Landing's top row, as the reader sees it: the Notice is inset from the
/// terminal edge exactly as the Landing's other rows are.
fn notice_row(rows: &[String]) -> String {
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
fn a_syntax_broken_config_document_notices_the_failure_and_points_at_the_log() {
    let workspace = workspace_dir();
    let application = landing_showing(workspace.path(), vec![unreadable_document()]);

    let landing = rendered_application_rows(&application);
    let notice = notice_row(&landing);
    assert!(
        notice.starts_with("× "),
        "a whole Config Document being ignored notices at error severity: {notice:?}"
    );
    assert!(
        notice.contains("suru.jsonc") && notice.contains("not valid JSONC"),
        "the Notice names the file and why it was ignored: {notice:?}"
    );
    assert!(
        notice.contains("see the Log"),
        "the Notice points at the Log, where the whole diagnostic is: {notice:?}"
    );
    assert!(
        landing
            .join("\n")
            .contains("What would you like to work on?"),
        "the Notice sits above the Landing rather than replacing it: {landing:?}"
    );
}

#[test]
fn an_ignored_duplicate_config_document_notices_the_file_it_dropped() {
    let workspace = workspace_dir();
    let application = landing_showing(
        workspace.path(),
        vec![SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: config_file("suru.json"),
            key: None,
            message: "ignored because suru.jsonc exists and wins".to_owned(),
        }],
    );

    let notice = notice_row(&rendered_application_rows(&application));
    assert!(
        notice.starts_with("! "),
        "an ignored duplicate is a warning, not a failure: {notice:?}"
    );
    assert!(
        notice.contains("suru.json ignored because suru.jsonc exists and wins"),
        "the Notice names the dropped file and what beat it: {notice:?}"
    );
}

#[test]
fn per_key_ignores_are_counted_in_the_notice_summary() {
    let workspace = workspace_dir();
    let application = landing_showing(workspace.path(), vec![unknown_key(), mistyped_key()]);

    let notice = notice_row(&rendered_application_rows(&application));
    assert!(
        notice.starts_with("! "),
        "keys ignored one at a time are a warning: {notice:?}"
    );
    assert!(
        notice.contains("2 keys ignored in suru.jsonc"),
        "the Notice counts the ignored keys and names their file: {notice:?}"
    );
    assert!(
        notice.contains("see the Log"),
        "the Log is where the key paths and reasons are: {notice:?}"
    );
}

#[test]
fn a_file_level_failure_is_worded_ahead_of_the_keys_it_shares_the_notice_with() {
    let workspace = workspace_dir();
    let application = landing_showing(
        workspace.path(),
        vec![unknown_key(), unreadable_document(), mistyped_key()],
    );

    let notice = notice_row(&rendered_application_rows_at(&application, 160, 15));
    let failure = notice
        .find("not valid JSONC")
        .unwrap_or_else(|| panic!("Notice omitted the file-level failure: {notice:?}"));
    let keys = notice
        .find("2 keys ignored")
        .unwrap_or_else(|| panic!("Notice omitted the per-key count: {notice:?}"));
    assert!(
        failure < keys,
        "the file-level failure is worded loudest, ahead of the key count: {notice:?}"
    );
    assert!(
        notice.starts_with("× "),
        "one error among warnings notices at error severity: {notice:?}"
    );
}

#[test]
fn an_ignored_document_is_worded_ahead_of_a_document_merely_dropped() {
    let workspace = workspace_dir();
    let application = landing_showing(
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

    let notice = notice_row(&rendered_application_rows_at(&application, 160, 15));
    let failure = notice
        .find("not valid JSONC")
        .unwrap_or_else(|| panic!("Notice omitted the failure: {notice:?}"));
    let dropped = notice
        .find("exists and wins")
        .unwrap_or_else(|| panic!("Notice omitted the dropped duplicate: {notice:?}"));
    assert!(
        failure < dropped,
        "the loudest failure leads, whatever order the loader found them in: {notice:?}"
    );
}

#[test]
fn a_summary_too_long_for_the_terminal_gives_way_before_the_pointer_at_the_log_does() {
    let workspace = workspace_dir();
    let application = landing_showing(
        workspace.path(),
        vec![unreadable_document(), unknown_key(), mistyped_key()],
    );

    for width in [40, 60, 80] {
        let notice = notice_row(&rendered_application_rows_at(&application, width, 15));
        assert!(
            notice.starts_with("× ") && notice.ends_with("see the Log"),
            "at width {width} the Notice still tells the reader where to look: {notice:?}"
        );
    }
}

#[test]
fn an_interaction_the_landing_makes_nothing_of_still_dismisses_the_notice() {
    let unbound_key = InputEvent::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
    let click = InputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 4,
        modifiers: KeyModifiers::NONE,
    });
    for interaction in [unbound_key, click] {
        let workspace = workspace_dir();
        let mut application = landing_showing(workspace.path(), vec![unreadable_document()]);
        rendered_application_rows(&application);

        application
            .handle_terminal_event(interaction.clone())
            .expect("deliver an interaction the Landing maps to no command");

        let dismissed = rendered_application_rows(&application).join("\n");
        assert!(
            !dismissed.contains("see the Log"),
            "{interaction:?} is an interaction even where it commands nothing: {dismissed}"
        );
    }
}

#[test]
fn the_terminal_resizing_around_the_reader_leaves_the_notice_standing() {
    let workspace = workspace_dir();
    let mut application = landing_showing(workspace.path(), vec![unreadable_document()]);

    application
        .handle_terminal_event(InputEvent::Resize(100, 30))
        .expect("deliver a resize");

    let landing = rendered_application_rows_at(&application, 100, 30).join("\n");
    assert!(
        landing.contains("see the Log"),
        "a resize is the terminal's doing, not the reader's: {landing}"
    );
}

#[test]
fn a_clean_configuration_renders_no_notice() {
    let workspace = workspace_dir();
    let application = landing_showing(workspace.path(), Vec::new());

    let landing = rendered_application_rows(&application);
    assert!(
        !landing.join("\n").contains("see the Log"),
        "a startup with nothing to report says nothing: {landing:?}"
    );
    assert_eq!(
        landing,
        rendered_application_rows(&connected_application(workspace.path())),
        "a clean snapshot leaves the Landing exactly as it was"
    );
}

#[test]
fn the_next_interaction_dismisses_the_notice_for_the_rest_of_the_run() {
    let workspace = workspace_dir();
    let mut application = landing_showing(workspace.path(), vec![unreadable_document()]);
    assert!(
        notice_row(&rendered_application_rows(&application)).contains("see the Log"),
        "the Notice shows before the user has touched anything"
    );

    type_terminal_text(&mut application, "hello");

    let dismissed = rendered_application_rows(&application).join("\n");
    assert!(
        !dismissed.contains("see the Log"),
        "the reader's next interaction dismisses the Notice: {dismissed}"
    );
    assert!(
        dismissed.contains("hello"),
        "the Notice never blocks input — the keystroke that dismissed it still typed: {dismissed}"
    );

    deliver_snapshot(&mut application, vec![unreadable_document()]);
    let reconnected = rendered_application_rows(&application).join("\n");
    assert!(
        !reconnected.contains("see the Log"),
        "a Notice the reader dismissed does not come back during the run: {reconnected}"
    );
}
