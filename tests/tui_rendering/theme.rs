//! A pinned built-in Theme paints the whole Application and changes on the
//! settings snapshot boundary shared by every attached Client.

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use suru::{
    protocol::{
        AppearanceMode, AppearanceSettings, EffectiveSettings, SettingMutation, SidebarSettings,
        SidebarVisibility,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        TerminalColor, TerminalColorProbe, TerminalFacts,
    },
};

use crate::support::{
    connected_application, connected_application_with_terminal_facts, deliver_settings,
    enter_session, rendered_application_buffer, rendered_application_rows, text_position,
    workspace_dir,
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
        aura.cell(text_position(&aura, "Type a Prompt and press Enter"))
            .unwrap()
            .bg,
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
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
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
    let light = rendered_application_buffer(&application, 100, 20);
    assert_eq!(light.cell((99, 0)).unwrap().bg, Color::Rgb(239, 241, 245));
    assert_eq!(
        light
            .cell(text_position(&light, "What would you like to work on?"))
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
    let placeholder = text_position(&buffer, "Type a Prompt and press Enter");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(37, 37, 37));
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::Rgb(60, 60, 60));
    assert_eq!(
        buffer.cell(composer_corner).unwrap().fg,
        Color::Rgb(29, 29, 29)
    );
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_paints_light_terminal_surfaces_borders_and_muted_text_light() {
    let application = probed_system_application(
        TerminalColor::new(255, 255, 255),
        TerminalColor::new(34, 34, 34),
    );
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a Prompt and press Enter");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(238, 238, 238));
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::Rgb(75, 75, 75));
    assert_eq!(
        buffer.cell(composer_corner).unwrap().fg,
        Color::Rgb(187, 187, 187)
    );
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_paints_dark_terminal_surfaces_borders_and_muted_text_dark() {
    let application = probed_system_application(
        TerminalColor::new(0, 0, 0),
        TerminalColor::new(238, 238, 238),
    );
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a Prompt and press Enter");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Rgb(17, 17, 17));
    assert_eq!(
        buffer.cell(placeholder).unwrap().fg,
        Color::Rgb(180, 180, 180)
    );
    assert_eq!(
        buffer.cell(composer_corner).unwrap().fg,
        Color::Rgb(68, 68, 68)
    );
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn an_unprobed_system_renders_with_the_original_terminal_colors() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    let buffer = rendered_application_buffer(&application, 100, 20);
    let placeholder = text_position(&buffer, "Type a Prompt and press Enter");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Black);
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::DarkGray);
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
}

#[test]
fn system_keeps_the_original_theme_when_no_background_was_observed() {
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
    let placeholder = text_position(&buffer, "Type a Prompt and press Enter");
    let composer_corner = (placeholder.0 - 2, placeholder.1 - 1);

    assert_eq!(buffer.cell((0, 0)).unwrap().bg, Color::Black);
    assert_eq!(buffer.cell(placeholder).unwrap().fg, Color::DarkGray);
    assert_eq!(buffer.cell(composer_corner).unwrap().fg, Color::Cyan);
    assert_eq!(buffer.cell((99, 0)).unwrap().bg, Color::Reset);
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

    let indexed = rendered_application_buffer(&indexed, 100, 20);
    assert!(
        indexed.content().iter().all(|cell| {
            !matches!(cell.fg, Color::Rgb(_, _, _)) && !matches!(cell.bg, Color::Rgb(_, _, _))
        }),
        "a terminal without truecolor receives no RGB Theme colors"
    );
    assert_eq!(
        indexed
            .cell(text_position(&indexed, "What would you like to work on?"))
            .unwrap()
            .fg,
        Color::Indexed(189),
        "Catppuccin text is nearest to xterm-256 slot 189"
    );

    let mut truecolor =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(&mut truecolor, themed("catppuccin"));

    let truecolor = rendered_application_buffer(&truecolor, 100, 20);
    assert_eq!(
        truecolor
            .cell(text_position(&truecolor, "What would you like to work on?"))
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

    let buffer = rendered_application_buffer(&application, 100, 20);
    assert_eq!(
        buffer
            .cell(text_position(&buffer, "What would you like to work on?"))
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
