//! A pinned built-in Theme paints the whole Application and changes on the
//! settings snapshot boundary shared by every attached Client.

use std::fs;

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde_json::json;
use suru::{
    protocol::{
        AppearanceMode, AppearanceSettings, EffectiveSettings, Outlook, Remote, RemoteHealth,
        RemoteStatus, SettingMutation, SidebarSettings, SidebarVisibility, Way,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        TerminalColor, TerminalColorProbe, TerminalFacts,
    },
};

use crate::support::{
    connected_application, connected_application_with_terminal_facts, deliver_settings,
    enter_session, rendered_application_buffer, rendered_application_rows,
    rendered_application_rows_at, text_position, type_terminal_text, workspace_dir,
};
use crate::transcript::{application_with_ansi_palette_output, assert_ansi_palette};

fn themed(name: &str) -> EffectiveSettings {
    themed_in_mode(name, AppearanceMode::System)
}

fn themed_in_mode(name: &str, mode: AppearanceMode) -> EffectiveSettings {
    EffectiveSettings {
        appearance: AppearanceSettings {
            theme: name.to_owned(),
            mode,
            landing_page: suru::protocol::LandingPage::Fancy,
            show_icons: false,
        },
        ..EffectiveSettings::default()
    }
}

fn open_theme_picker(application: &mut Application) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ThemeList,
        )))
        .expect("open Theme picker")
}

/// Keeps Theme preview assertions on the full main-view canvas after a
/// snapshot seeds the independently configured Sidebar as shown.
fn hide_sidebar_as_view_state(application: &mut Application) {
    // The Setting's reveal leaves the keys with the composer, so the first
    // toggle only reaches the Sidebar and the second hides it.
    for _ in 0..2 {
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    SemanticCommandId::SidebarToggle,
                )))
                .expect("hide the Sidebar without changing effective Settings"),
            ApplicationTransition::Continue
        );
    }
}

fn truecolor_application() -> Application {
    let mut application = Application::default();
    application.set_terminal_facts(TerminalFacts::unprobed(true));
    application
}

fn probed_system_application(background: TerminalColor, foreground: TerminalColor) -> Application {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(
        workspace.path(),
        TerminalFacts::new(
            Some(TerminalColorProbe::new(
                [None; 16],
                Some(foreground),
                Some(background),
            )),
            true,
        ),
    );
    deliver_settings(&mut application, EffectiveSettings::default());
    application
}

fn press(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("handle Theme picker key")
}

#[test]
fn picker_orders_system_first_then_the_built_ins_alphabetically() {
    let mut application = Application::default();
    assert_eq!(
        open_theme_picker(&mut application),
        ApplicationTransition::Continue
    );

    let picker = rendered_application_rows(&application).join("\n");
    let system = picker.find("System").expect("System row");
    let aura = picker.find("aura").expect("Aura row");
    let ayu = picker.find("ayu").expect("Ayu row");
    let carbonfox = picker.find("carbonfox").expect("Carbonfox row");
    assert!(system < aura && aura < ayu && ayu < carbonfox, "{picker}");
}

#[test]
fn a_user_theme_is_tagged_previewed_and_pinned_by_its_basename() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let themes = config_root.path().join("themes");
    fs::create_dir(&themes).expect("create themes directory");
    fs::write(
        themes.join("reader.json"),
        include_str!("../../src/theme/assets/aura.json"),
    )
    .expect("write user Theme");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(&mut application, EffectiveSettings::default());

    open_theme_picker(&mut application);
    for character in "reader".chars() {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }

    let rows = rendered_application_rows(&application).join("\n");
    assert!(rows.contains("reader · [user]"), "{rows}");
    let preview = rendered_application_buffer(&application, 100, 20);
    assert_eq!(preview.cell((0, 0)).unwrap().bg, Color::Rgb(21, 20, 27));
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceTheme {
            value: Some("reader".to_owned()),
        })
    );
}

#[test]
fn a_user_theme_shadows_a_built_in_with_the_same_basename() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let themes = config_root.path().join("themes");
    fs::create_dir(&themes).expect("create themes directory");
    fs::write(
        themes.join("catppuccin.json"),
        include_str!("../../src/theme/assets/aura.json"),
    )
    .expect("write shadowing user Theme");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(&mut application, EffectiveSettings::default());

    open_theme_picker(&mut application);
    for character in "catppuccin".chars() {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }

    let rows = rendered_application_rows(&application).join("\n");
    assert_eq!(rows.matches("catppuccin ·").count(), 1, "{rows}");
    assert!(rows.contains("catppuccin · [user]"), "{rows}");
    let preview = rendered_application_buffer(&application, 100, 20);
    assert_eq!(preview.cell((0, 0)).unwrap().bg, Color::Rgb(21, 20, 27));
}

#[test]
fn rejected_user_theme_files_are_noticed_without_blocking_the_directory() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let themes = config_root.path().join("themes");
    fs::create_dir(&themes).expect("create themes directory");
    let aura = include_str!("../../src/theme/assets/aura.json");
    fs::write(themes.join("aad-valid.json"), aura).expect("write valid user Theme");
    fs::write(themes.join("aaa-broken.json"), "{").expect("write broken user Theme");
    let mut unresolved: serde_json::Value = serde_json::from_str(aura).expect("parse fixture");
    unresolved["theme"]["primary"] = json!("does-not-exist");
    fs::write(themes.join("aab-unresolved.json"), unresolved.to_string())
        .expect("write unresolved user Theme");
    let mut version_two: serde_json::Value = serde_json::from_str(aura).expect("parse fixture");
    version_two["version"] = json!(2);
    fs::write(themes.join("aac-future.json"), version_two.to_string())
        .expect("write future user Theme");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(
        &mut application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Hidden,
                ..SidebarSettings::default()
            },
            ..EffectiveSettings::default()
        },
    );

    let notice = rendered_application_rows_at(&application, 240, 15)[0].clone();
    assert!(
        notice.contains("aaa-broken.json") && notice.contains("not valid JSON"),
        "{notice}"
    );
    assert!(
        notice.contains("aab-unresolved.json")
            && notice.contains("colors could not be resolved")
            && !notice.contains("does-not-exist"),
        "{notice}"
    );
    assert!(
        notice.contains("aac-future.json") && notice.contains("version 2"),
        "{notice}"
    );

    open_theme_picker(&mut application);
    let picker = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(picker.contains("aad-valid · [user]"), "{picker}");
    assert!(!picker.contains("aaa-broken · [user]"), "{picker}");
    assert!(!picker.contains("aab-unresolved · [user]"), "{picker}");
    assert!(!picker.contains("aac-future · [user]"), "{picker}");
}

#[test]
fn a_user_file_cannot_replace_the_terminal_derived_system_theme() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let themes = config_root.path().join("themes");
    fs::create_dir(&themes).expect("create themes directory");
    fs::write(
        themes.join("system.json"),
        include_str!("../../src/theme/assets/aura.json"),
    )
    .expect("write reserved user Theme");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(
        &mut application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Hidden,
                ..SidebarSettings::default()
            },
            ..EffectiveSettings::default()
        },
    );

    let notice = rendered_application_rows_at(&application, 120, 15)[0].clone();
    assert!(
        notice.contains("system.json") && notice.contains("System is reserved"),
        "{notice}"
    );
    open_theme_picker(&mut application);
    let picker = rendered_application_rows(&application).join("\n");
    assert_eq!(picker.matches("System ·").count(), 1, "{picker}");
    assert!(!picker.contains("System · [user]"), "{picker}");
    let system = rendered_application_buffer(&application, 100, 20);
    assert!(
        system
            .content()
            .iter()
            .all(|cell| !matches!(cell.bg, Color::Rgb(_, _, _)))
    );
}

#[test]
fn opening_the_picker_finds_a_new_file_and_restores_its_fallen_back_pin() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let themes = config_root.path().join("themes");
    fs::create_dir(&themes).expect("create themes directory");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(&mut application, themed("restored"));

    let fallback = rendered_application_buffer(&application, 100, 20);
    assert!(
        fallback
            .content()
            .iter()
            .all(|cell| !matches!(cell.bg, Color::Rgb(_, _, _)))
    );
    assert!(
        rendered_application_rows(&application)[0].contains("restored"),
        "the missing pin is noticed before its file returns"
    );

    fs::write(
        themes.join("restored.json"),
        include_str!("../../src/theme/assets/aura.json"),
    )
    .expect("restore pinned user Theme");
    open_theme_picker(&mut application);

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("restored · [user] · [current]"), "{picker}");
    let restored = rendered_application_buffer(&application, 100, 20);
    assert_eq!(restored.cell((0, 0)).unwrap().bg, Color::Rgb(21, 20, 27));
}

#[test]
fn a_remote_outlook_keeps_reading_the_clients_config_root() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().expect("create config root");
    let local_themes = config_root.path().join("themes");
    fs::create_dir(&local_themes).expect("create local themes directory");
    fs::write(
        local_themes.join("local-reader.json"),
        include_str!("../../src/theme/assets/aura.json"),
    )
    .expect("write local user Theme");
    let other_themes = workspace.path().join("themes");
    fs::create_dir(&other_themes).expect("create unrelated themes directory");
    fs::write(
        other_themes.join("remote-reader.json"),
        include_str!("../../src/theme/assets/ayu.json"),
    )
    .expect("write unrelated Theme");
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
        .with_config_root(config_root.path());
    deliver_settings(&mut application, EffectiveSettings::default());

    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            ways: vec![Way::Direct(
                "10.0.0.8:7777".parse().expect("parse remote address"),
            )],
            status: RemoteStatus::Available,
        }]))
        .expect("list Remotes");
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(1),
                status: RemoteStatus::Available,
            }),
        })
        .expect("probe Remote");
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert!(matches!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote(name),
            ..
        } if name == "studio"
    ));

    open_theme_picker(&mut application);
    for character in "local-reader".chars() {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }
    let local = rendered_application_rows(&application).join("\n");
    assert!(local.contains("local-reader · [user]"), "{local}");

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    open_theme_picker(&mut application);
    for character in "remote-reader".chars() {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }
    let remote = rendered_application_rows(&application).join("\n");
    assert!(remote.contains("No Themes found"), "{remote}");
}

#[test]
fn moving_and_filtering_preview_the_top_match_without_leaving_the_client() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut application, EffectiveSettings::default());
    enter_session(&mut application, workspace.path());
    open_theme_picker(&mut application);

    assert_eq!(
        press(&mut application, KeyCode::Down, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    let aura = rendered_application_buffer(&application, 100, 20);
    assert_eq!(aura.cell((99, 0)).unwrap().bg, Color::Rgb(15, 15, 15));
    assert_eq!(aura.cell((0, 0)).unwrap().bg, Color::Rgb(21, 20, 27));
    assert_eq!(
        aura.cell(text_position(&aura, "Type a prompt")).unwrap().bg,
        Color::Rgb(15, 15, 15),
        "the composer repaints with the preview"
    );

    assert_eq!(
        press(&mut application, KeyCode::Down, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    let ayu = rendered_application_buffer(&application, 100, 20);
    assert_eq!(ayu.cell((99, 0)).unwrap().bg, Color::Rgb(11, 14, 20));
    assert_eq!(ayu.cell((0, 0)).unwrap().bg, Color::Rgb(15, 19, 26));

    for character in ['c', 'a', 't'] {
        assert_eq!(
            press(
                &mut application,
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ),
            ApplicationTransition::Continue
        );
    }
    let filtered = rendered_application_buffer(&application, 100, 20);
    assert_eq!(filtered.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));
    let rows = rendered_application_rows(&application).join("\n");
    assert!(rows.contains("catppuccin"));
    assert!(!rows.contains("carbonfox"));

    for _ in 0..3 {
        assert_eq!(
            press(&mut application, KeyCode::Backspace, KeyModifiers::NONE),
            ApplicationTransition::Continue
        );
    }
    let restored = rendered_application_buffer(&application, 100, 20);
    assert_eq!(restored.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn escape_restores_the_opening_theme_and_enter_holds_preview_until_snapshot() {
    let mut application = truecolor_application();
    open_theme_picker(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Rgb(15, 15, 15)
    );

    assert_eq!(
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Reset
    );

    open_theme_picker(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceTheme {
            value: Some("aura".to_owned()),
        })
    );
    let awaiting_snapshot = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        awaiting_snapshot.cell((0, 0)).unwrap().bg,
        Color::Rgb(15, 15, 15)
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(" Themes ")
    );

    deliver_settings(&mut application, EffectiveSettings::default());
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Reset
    );
}

#[test]
fn an_unrelated_snapshot_does_not_end_a_preview_still_being_browsed() {
    let mut application = truecolor_application();
    open_theme_picker(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);

    deliver_settings(&mut application, EffectiveSettings::default());
    hide_sidebar_as_view_state(&mut application);
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Rgb(15, 15, 15)
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceTheme {
            value: Some("aura".to_owned()),
        })
    );
}

#[test]
fn escape_restores_the_opening_theme_after_another_client_changes_the_pin() {
    let mut application = truecolor_application();
    open_theme_picker(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);

    deliver_settings(&mut application, themed("catppuccin"));
    hide_sidebar_as_view_state(&mut application);
    assert_eq!(
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Reset,
        "Escape restores System, which was in force when the picker opened"
    );

    deliver_settings(&mut application, themed("catppuccin"));
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Rgb(30, 30, 46),
        "a later authoritative snapshot can move the Client on"
    );
}

#[test]
fn reopening_before_the_snapshot_keeps_the_confirmed_preview_as_the_opening_theme() {
    let mut application = truecolor_application();
    open_theme_picker(&mut application);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert!(matches!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(_)
    ));

    open_theme_picker(&mut application);
    let reopened = rendered_application_rows(&application).join("\n");
    assert!(reopened.contains("aura · [current]"), "{reopened}");
    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Rgb(15, 15, 15),
        "the held aura preview remains the Theme in force until its snapshot"
    );
}

#[test]
fn choosing_system_pins_system_and_immediately_previews_terminal_colors() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, themed("catppuccin"));
    open_theme_picker(&mut application);
    for character in ['s', 'y', 's'] {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }

    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceTheme {
            value: Some("system".to_owned()),
        })
    );
    assert_eq!(
        rendered_application_buffer(&application, 80, 15)
            .cell((0, 0))
            .unwrap()
            .bg,
        Color::Reset
    );
}

#[test]
fn enter_cannot_confirm_a_theme_filtered_out_of_the_list() {
    let mut application = Application::default();
    open_theme_picker(&mut application);
    for character in ['q', 'q', 'q'] {
        press(
            &mut application,
            KeyCode::Char(character),
            KeyModifiers::NONE,
        );
    }
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("No Themes found")
    );

    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(" Themes ")
    );
}

#[test]
fn a_snapshot_repaints_an_open_view_and_its_overlay_in_the_named_theme() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
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
            .cell(text_position(&session, "Type a prompt"))
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
    assert_eq!(
        after.cell(theme_row).unwrap().fg,
        Color::Rgb(30, 30, 46),
        "the focused row's text takes the Theme's selected list item colour"
    );
    assert_eq!(
        after.cell(theme_row).unwrap().bg,
        Color::Rgb(137, 180, 250),
        "over a block in the Theme's primary colour, not the crust the panel is already painted"
    );
    let settings_title = text_position(&after, "Settings");
    assert_eq!(
        after.cell(settings_title).unwrap().bg,
        Color::Rgb(17, 17, 27)
    );
}

#[test]
fn a_named_theme_paints_ordinary_text_with_its_own_foreground() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut application, themed("catppuccin"));

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Theme sample".to_owned(),
        )))
        .expect("type sample text");
    let landing = rendered_application_buffer(&application, 100, 20);
    assert_eq!(
        landing
            .cell(text_position(&landing, "▀▀▀▀▀▀▀▀█▀▀▀▀▀"))
            .unwrap()
            .fg,
        Color::Rgb(137, 180, 250),
        "the Landing logo keeps the primary accent"
    );
    assert_eq!(
        landing
            .cell(text_position(&landing, "Theme sample"))
            .unwrap()
            .fg,
        Color::Rgb(205, 214, 244)
    );
}

#[test]
fn a_two_variant_theme_repaints_when_mode_changes() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));

    deliver_settings(
        &mut application,
        themed_in_mode("catppuccin", AppearanceMode::Dark),
    );
    let dark = rendered_application_buffer(&application, 100, 20);
    assert_eq!(dark.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));

    deliver_settings(
        &mut application,
        themed_in_mode("catppuccin", AppearanceMode::Light),
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Theme sample".to_owned(),
        )))
        .expect("type sample text");
    let light = rendered_application_buffer(&application, 100, 20);
    assert_eq!(light.cell((99, 0)).unwrap().bg, Color::Rgb(239, 241, 245));
    assert_eq!(
        light
            .cell(text_position(&light, "Theme sample"))
            .unwrap()
            .fg,
        Color::Rgb(76, 79, 105)
    );
}

#[test]
fn a_single_variant_theme_does_not_repaint_when_mode_changes() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(
        &mut application,
        themed_in_mode("aura", AppearanceMode::Dark),
    );
    let dark = rendered_application_buffer(&application, 100, 20);

    deliver_settings(
        &mut application,
        themed_in_mode("aura", AppearanceMode::Light),
    );
    let light = rendered_application_buffer(&application, 100, 20);
    assert_eq!(light, dark);
}

#[test]
fn a_pinned_user_theme_uses_effective_mode_on_the_first_frame_and_after_late_colors() {
    let workspace = workspace_dir();
    let config_root = tempfile::tempdir().unwrap();
    fs::create_dir(config_root.path().join("themes")).unwrap();
    fs::write(
        config_root.path().join("themes/reader.json"),
        include_str!("../../src/theme/assets/catppuccin.json"),
    )
    .unwrap();
    for (mode, initial_background, final_background) in [
        (
            AppearanceMode::Dark,
            Color::Rgb(30, 30, 46),
            Color::Rgb(30, 30, 46),
        ),
        (
            AppearanceMode::Light,
            Color::Rgb(239, 241, 245),
            Color::Rgb(239, 241, 245),
        ),
        (
            AppearanceMode::System,
            Color::Rgb(30, 30, 46),
            Color::Rgb(239, 241, 245),
        ),
    ] {
        let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true))
            .with_config_root(config_root.path());
        deliver_settings(&mut application, themed_in_mode("reader", mode));
        let initial = rendered_application_buffer(&application, 100, 20);
        assert_eq!(initial.cell((99, 0)).unwrap().bg, initial_background);
        application.set_terminal_facts(TerminalFacts::new(
            Some(TerminalColorProbe::new(
                [None; 16],
                None,
                Some(TerminalColor::new(255, 255, 255)),
            )),
            true,
        ));
        let updated = rendered_application_buffer(&application, 100, 20);
        assert_eq!(updated.cell((99, 0)).unwrap().bg, final_background);
    }
}

#[test]
fn an_unprobed_terminal_uses_the_dark_variant_in_system_mode() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut application, themed("catppuccin"));

    let buffer = rendered_application_buffer(&application, 100, 20);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));
}

#[test]
fn system_mode_follows_the_terminal_background_luminance() {
    let workspace = workspace_dir();
    let facts = |background| {
        TerminalFacts::new(
            Some(TerminalColorProbe::new([None; 16], None, Some(background))),
            true,
        )
    };
    let mut application = connected_application_with_terminal_facts(
        workspace.path(),
        facts(TerminalColor::new(0, 0, 0)),
    );
    deliver_settings(&mut application, themed("catppuccin"));
    let dark = rendered_application_buffer(&application, 100, 20);
    assert_eq!(dark.cell((99, 0)).unwrap().bg, Color::Rgb(30, 30, 46));

    application.set_terminal_facts(facts(TerminalColor::new(255, 255, 255)));
    let light = rendered_application_buffer(&application, 100, 20);
    assert_eq!(light.cell((99, 0)).unwrap().bg, Color::Rgb(239, 241, 245));
}

#[test]
fn system_theme_uses_a_locked_light_ramp_against_a_dark_terminal_background() {
    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(
        workspace.path(),
        TerminalFacts::new(
            Some(TerminalColorProbe::new(
                [None; 16],
                Some(TerminalColor::new(238, 238, 238)),
                Some(TerminalColor::new(40, 40, 40)),
            )),
            true,
        ),
    );
    deliver_settings(
        &mut application,
        themed_in_mode("system", AppearanceMode::Light),
    );
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a prompt");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(37, 37, 37));
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::Rgb(60, 60, 60));
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_paints_light_terminal_surfaces_borders_and_muted_text_light() {
    let application = probed_system_application(
        TerminalColor::new(255, 255, 255),
        TerminalColor::new(34, 34, 34),
    );
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a prompt");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(238, 238, 238));
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::Rgb(75, 75, 75));
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_paints_dark_terminal_surfaces_borders_and_muted_text_dark() {
    let application = probed_system_application(
        TerminalColor::new(0, 0, 0),
        TerminalColor::new(238, 238, 238),
    );
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a prompt");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(17, 17, 17));
    assert_eq!(
        buffer.cell(placeholder).unwrap().fg,
        Color::Rgb(180, 180, 180)
    );
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn an_unprobed_system_leaves_panel_backgrounds_to_the_terminal() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a prompt");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Reset);
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::DarkGray);
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_leaves_panel_backgrounds_to_the_terminal_until_background_is_observed() {
    let workspace = workspace_dir();
    let mut palette = [None; 16];
    palette[1] = Some(TerminalColor::new(12, 34, 56));
    let facts = TerminalFacts::new(
        Some(TerminalColorProbe::new(
            palette,
            Some(TerminalColor::new(238, 238, 238)),
            None,
        )),
        true,
    );
    let mut application = connected_application_with_terminal_facts(workspace.path(), facts);
    deliver_settings(&mut application, EffectiveSettings::default());
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a prompt");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Reset);
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::DarkGray);
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_keeps_terminal_defaults_through_palette_replies_then_repaints_for_a_light_background() {
    let workspace = workspace_dir();
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut application, EffectiveSettings::default());
    let initial = rendered_application_buffer(&application, 100, 20);
    let mut palette = [None; 16];
    palette[0] = Some(TerminalColor::new(0, 0, 0));
    application.set_terminal_facts(TerminalFacts::new(
        Some(TerminalColorProbe::new(palette, None, None)),
        true,
    ));
    assert_eq!(rendered_application_buffer(&application, 100, 20), initial);

    application.set_terminal_facts(TerminalFacts::new(
        Some(TerminalColorProbe::new(
            palette,
            Some(TerminalColor::new(34, 34, 34)),
            Some(TerminalColor::new(255, 255, 255)),
        )),
        true,
    ));
    let light = rendered_application_buffer(&application, 100, 20);
    assert_eq!(light.cell((0, 0)).unwrap().bg, Color::Rgb(238, 238, 238));
    assert_eq!(light.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn unreported_system_palette_slots_keep_the_static_ansi_colors() {
    let workspace = workspace_dir();
    let mut palette = [None; 16];
    palette[1] = Some(TerminalColor::new(12, 34, 56));
    let facts = TerminalFacts::new(
        Some(TerminalColorProbe::new(
            palette,
            Some(TerminalColor::new(238, 238, 238)),
            Some(TerminalColor::new(0, 0, 0)),
        )),
        true,
    );
    let application = application_with_ansi_palette_output(workspace.path(), facts);
    let buffer = rendered_application_buffer(&application, 160, 30);
    let normal = [
        Color::Black,
        Color::Rgb(12, 34, 56),
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    let bright = [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];

    assert_ansi_palette(&buffer, &normal, &bright);
}

#[test]
fn a_named_theme_follows_the_terminals_truecolor_capability() {
    let workspace = workspace_dir();
    let mut indexed =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(false));
    deliver_settings(&mut indexed, themed("catppuccin"));

    indexed
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Theme sample".to_owned(),
        )))
        .expect("type sample text");
    let indexed = rendered_application_buffer(&indexed, 100, 20);
    assert!(
        indexed.content().iter().all(|cell| {
            !matches!(cell.fg, Color::Rgb(_, _, _)) && !matches!(cell.bg, Color::Rgb(_, _, _))
        }),
        "a terminal without truecolor receives no RGB Theme colors"
    );
    assert_eq!(
        indexed
            .cell(text_position(&indexed, "Theme sample"))
            .unwrap()
            .fg,
        Color::Indexed(189),
        "Catppuccin text is nearest to xterm-256 slot 189"
    );

    let mut truecolor =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut truecolor, themed("catppuccin"));

    truecolor
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Theme sample".to_owned(),
        )))
        .expect("type sample text");
    let truecolor = rendered_application_buffer(&truecolor, 100, 20);
    assert_eq!(
        truecolor
            .cell(text_position(&truecolor, "Theme sample"))
            .unwrap()
            .fg,
        Color::Rgb(205, 214, 244)
    );
}

#[test]
fn a_named_theme_quantizes_against_the_terminals_reported_indexed_colors() {
    let workspace = workspace_dir();
    let mut palette = [None; 16];
    palette[1] = Some(TerminalColor::new(205, 214, 244));
    let facts = TerminalFacts::new(Some(TerminalColorProbe::new(palette, None, None)), false);
    let mut application = connected_application_with_terminal_facts(workspace.path(), facts);
    deliver_settings(&mut application, themed("catppuccin"));

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Theme sample".to_owned(),
        )))
        .expect("type sample text");
    let buffer = rendered_application_buffer(&application, 100, 20);
    assert_eq!(
        buffer
            .cell(text_position(&buffer, "Theme sample"))
            .unwrap()
            .fg,
        Color::Indexed(1),
        "an exact terminal palette match wins over an xterm-256 approximation"
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
            ..AppearanceSettings::default()
        },
        sidebar: SidebarSettings {
            initial_visibility: SidebarVisibility::Hidden,
            ..SidebarSettings::default()
        },
        aside: suru::protocol::AsideSettings {
            initial_visibility: suru::protocol::AsideVisibility::Hidden,
            ..suru::protocol::AsideSettings::default()
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
