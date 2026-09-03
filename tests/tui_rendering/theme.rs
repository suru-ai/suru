//! A pinned built-in Theme paints the whole Application and changes on the
//! settings snapshot boundary shared by every attached Client.

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use suru::protocol::{AppearanceSettings, EffectiveSettings, SidebarSettings, SidebarVisibility};

use crate::support::{
    connected_application, deliver_settings, enter_session, rendered_application_buffer,
    rendered_application_rows, text_position, workspace_dir,
};

fn themed(name: &str) -> EffectiveSettings {
    EffectiveSettings {
        appearance: AppearanceSettings {
            theme: name.to_owned(),
        },
        ..EffectiveSettings::default()
    }
}

#[test]
fn a_snapshot_repaints_an_open_view_and_its_overlay_in_the_named_theme() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    enter_session(&mut application, workspace.path());
    let before = rendered_application_buffer(&application, 100, 20);

    deliver_settings(&mut application, themed("catppuccin"));
    let session = rendered_application_buffer(&application, 100, 20);
    assert_eq!(session.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));
    assert_eq!(session.cell((0, 0)).unwrap().bg, Color::Rgb(24, 24, 37));
    assert_eq!(
        session
            .cell(text_position(&session, "Initial Prompt"))
            .unwrap()
            .bg,
        Color::Rgb(24, 24, 37),
        "Transcript cells use the Theme's elevated background"
    );
    assert_eq!(
        session
            .cell(text_position(&session, "Type a Prompt and press Enter"))
            .unwrap()
            .bg,
        Color::Rgb(30, 30, 46),
        "composer cells keep the Theme background"
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("open leader");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(','),
            KeyModifiers::NONE,
        )))
        .expect("open settings overlay");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )))
        .expect("pass Providers");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )))
        .expect("show Appearance");

    let after = rendered_application_buffer(&application, 100, 20);
    assert_ne!(before.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));
    assert_eq!(after.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));
    assert_eq!(after.cell((0, 0)).unwrap().bg, Color::Rgb(24, 24, 37));
    let theme_row = text_position(&after, "Theme · catppuccin");
    assert_eq!(after.cell(theme_row).unwrap().fg, Color::Rgb(30, 30, 46));
    assert_eq!(after.cell(theme_row).unwrap().bg, Color::Rgb(137, 180, 250));
    let settings_title = text_position(&after, "Settings");
    assert_eq!(
        after.cell(settings_title).unwrap().bg,
        Color::Rgb(17, 17, 27)
    );
}

#[test]
fn a_named_theme_paints_ordinary_text_with_its_own_foreground() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, themed("catppuccin"));

    let landing = rendered_application_buffer(&application, 100, 20);
    assert_eq!(
        landing
            .cell(text_position(&landing, "What would you like to work on?"))
            .unwrap()
            .fg,
        Color::Rgb(205, 214, 244)
    );
}

#[test]
fn a_theme_with_transparent_surfaces_resets_cell_backgrounds() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, themed("lucent-orng"));

    let buffer = rendered_application_buffer(&application, 100, 20);
    assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
}

#[test]
fn a_missing_theme_falls_back_to_system_and_notices_the_preserved_pin_in_an_open_session() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    enter_session(&mut application, workspace.path());
    let settings = EffectiveSettings {
        appearance: AppearanceSettings {
            theme: "gone-away".to_owned(),
        },
        sidebar: SidebarSettings {
            initial_visibility: SidebarVisibility::Hidden,
            ..SidebarSettings::default()
        },
        ..EffectiveSettings::default()
    };
    deliver_settings(&mut application, settings.clone());
    deliver_settings(&mut application, settings.clone());
    assert!(
        !application.note_interaction(&InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        ))),
        "input coalesced before the repaint cannot dismiss an unseen Notice"
    );

    let rows = rendered_application_rows(&application);
    assert!(
        rows[0].contains("gone-away") && rows[0].contains("using System"),
        "the runtime fallback names the pin it left untouched: {:?}",
        rows[0]
    );
    assert_eq!(
        rows[0].matches("gone-away").count(),
        1,
        "a repeated snapshot keeps one Notice for the unresolved pin"
    );
    let buffer = rendered_application_buffer(&application, 80, 15);
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| !matches!(cell.bg, Color::Rgb(_, _, _)))
    );

    assert!(application.note_interaction(&InputEvent::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
    ))));
    let mut next = settings;
    next.appearance.theme = "another-missing-theme".to_owned();
    deliver_settings(&mut application, next);
    assert!(rendered_application_rows(&application)[0].contains("another-missing-theme"));
    assert!(application.note_interaction(&InputEvent::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::NONE,
    ))));
    deliver_settings(&mut application, themed("gone-away"));
    assert!(!rendered_application_rows(&application)[0].contains("gone-away"));
}
