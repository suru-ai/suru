//! The settings panel: the tabs it splits its Settings across, and editing a
//! Setting from any of them.

use std::path::Path;

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};

use crate::support::{
    connected_application, model_descriptor, rendered_application_buffer,
    rendered_application_rows, rendered_application_rows_at, selected_session_snapshot,
    text_position, type_terminal_text, workspace_dir,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, AppearanceMode, AppearanceSettings, AutoSettle, CodexSettings,
        CopilotSettings, EffectiveSettings, EmojiVisibility, FoldPosture, ModelAvailability,
        ModelCatalog, ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor,
        ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, ProviderSettings,
        ProviderUnavailability, ReasoningSummaryDetail, ReasoningVisibility, SessionContentWidth,
        SessionId, SessionSettings, SettingMutation, SettingScope, SettingsSnapshot, SidebarScope,
        SidebarSettings, TitleErrand, TitleSettings, TranscriptSettings,
    },
    settings::SettingGroup,
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, ModelListRequest,
        NumericDigit, SemanticCommandId,
    },
};

/// The Spinner's first frame, which is what a row being read shows.
const SPINNER: char = '⠋';

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
            ..TranscriptSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// Effective settings whose only departure from the built-in defaults is that
/// the reader turned Copilot off.
fn without_copilot() -> EffectiveSettings {
    EffectiveSettings {
        provider: ProviderSettings {
            copilot: CopilotSettings { enabled: false },
            ..ProviderSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// Effective settings in which the reader turned Codex off, leaving the
/// Provider that carries further Settings the one that is disabled.
fn without_codex() -> EffectiveSettings {
    EffectiveSettings {
        provider: ProviderSettings {
            codex: CodexSettings {
                enabled: false,
                ..CodexSettings::default()
            },
            ..ProviderSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

fn with_content_width(content_width: SessionContentWidth) -> EffectiveSettings {
    EffectiveSettings {
        session: SessionSettings {
            content_width,
            ..SessionSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

fn with_sidebar_scope(initial_scope: SidebarScope) -> EffectiveSettings {
    EffectiveSettings {
        sidebar: SidebarSettings {
            initial_scope,
            ..SidebarSettings::default()
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

/// A left click on one cell of the panel, sent as a press and release.
fn click(application: &mut Application, column: u16, row: u16) -> ApplicationTransition {
    super::support::click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("handle a settings panel click")
}

/// Clicks the label the tab bar draws for `title`, which is where a reader
/// points to switch tabs.
fn click_tab(application: &mut Application, title: &str) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, 80, 15);
    let (column, row) = text_position(&buffer, title);
    click(application, column, row)
}

/// Clicks the row the panel draws for `label`, on the label itself.
fn click_row(application: &mut Application, label: &str) -> ApplicationTransition {
    let rows = rendered_application_rows(application);
    let row = row_index(&rows, label) as u16;
    let column = label_column(application, label) as u16;
    click(application, column, row)
}

fn open_panel(application: &mut Application) {
    press(application, KeyCode::Char('x'), KeyModifiers::CONTROL);
    press(application, KeyCode::Char(','), KeyModifiers::NONE);
}

/// The panel as it opens, on the Providers tab.
fn open_providers_tab(application: &mut Application) {
    open_panel(application);
    press(application, KeyCode::Right, KeyModifiers::NONE);
}

/// The panel as it opens, on the Experimental tab. Reached by wrapping
/// backwards off the first tab rather than by walking forwards onto it, so
/// arriving here never passes through the Providers and the read they begin.
fn open_experimental_tab(application: &mut Application) {
    open_panel(application);
    press(application, KeyCode::Left, KeyModifiers::NONE);
}

/// The Providers tab, plus the catalog listing entering it asks for — which is
/// how a test answers the read the panel has just begun.
fn read_providers_tab(application: &mut Application) -> ModelListRequest {
    open_panel(application);
    let ApplicationTransition::ListModels(request) =
        press(application, KeyCode::Right, KeyModifiers::NONE)
    else {
        panic!("entering the Providers tab reads every enabled Provider's Availability");
    };
    request
}

/// What a listing answers with: one entry per Provider named, carrying the
/// status alone, because the status is all a Provider row reads.
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

/// Answers a read with what it found, which is the only way anything about
/// Availability reaches a row.
fn deliver_catalog(
    application: &mut Application,
    request: ModelListRequest,
    providers: &[(&str, ProviderCatalogStatus)],
) {
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: catalog(providers),
        })
        .expect("receive what the Availability read found");
}

/// Walks the focus down to the Setting spelling `key`, which the panel names
/// in the focused row's headline. Navigating by the key rather than by a count
/// of presses keeps these tests off the row order of the tab, which is free to
/// grow.
fn focus_setting(application: &mut Application, key: &str) {
    for _ in 0..suru::settings::SCHEMA.len() {
        if rendered_application_rows(application)
            .join("\n")
            .contains(key)
        {
            return;
        }
        press(application, KeyCode::Down, KeyModifiers::NONE);
    }
    panic!("the settings panel never focused {key:?}");
}

/// The Setting the panel says it would edit: the focused row is the only one
/// whose key the panel spells, in its headline.
fn focused_key(application: &Application) -> String {
    let panel = rendered_application_rows(application).join("\n");
    let named = suru::settings::SCHEMA
        .iter()
        .map(|descriptor| descriptor.key)
        .filter(|key| panel.contains(key))
        .collect::<Vec<_>>();
    match named.as_slice() {
        [key] => (*key).to_owned(),
        _ => panic!("the settings panel named {named:?} rather than one focused Setting"),
    }
}

/// The panel row for one Setting or Provider, as the reader sees it. Every row
/// carries its provenance marker, which is what tells one apart from the
/// headline naming the same Setting in prose.
fn row(application: &Application, label: &str) -> String {
    let rows = rendered_application_rows(application);
    rows[row_index(&rows, label)].trim().to_owned()
}

/// Where that row sits on screen, so a test can say which row comes first.
fn row_index(rows: &[String], label: &str) -> usize {
    rows.iter()
        .position(|row| is_row_for(row, label))
        .unwrap_or_else(|| panic!("the settings panel showed no row for {label:?}: {rows:?}"))
}

/// Whether the panel is showing a row for `label` at all, which is how a test
/// says a Provider's further Settings are revealed or hidden. Read off the
/// provenance marker every row carries, so the headline naming the same
/// Setting in prose is never mistaken for a row of its own.
fn has_row(application: &Application, label: &str) -> bool {
    rendered_application_rows(application)
        .iter()
        .any(|row| is_row_for(row, label))
}

fn is_row_for(row: &str, label: &str) -> bool {
    row.contains(label) && (row.contains("[pinned]") || row.contains("[default]"))
}

/// Which column a row's name starts at, which is what says a revealed Setting
/// is drawn indented beneath the Provider it configures.
fn label_column(application: &Application, label: &str) -> usize {
    label_column_in(&rendered_application_rows(application), label)
}

/// The same column, read off a frame already in hand, so a test that rendered
/// at a size of its own points at what that frame drew.
fn label_column_in(rows: &[String], label: &str) -> usize {
    let row = &rows[row_index(rows, label)];
    let start = row
        .find(label)
        .expect("the row found by its label contains it");
    // Counted in cells rather than bytes: the markers a row leads with are
    // multi-byte, so a byte offset would read as an indent the reader never
    // sees.
    row[..start].chars().count()
}

/// How the panel drew a word, which is how a tab says it is the active one and
/// how a Provider row says it is disabled.
fn styling(buffer: &Buffer, needle: &str) -> (Color, Color, Modifier) {
    let cell = buffer
        .cell(text_position(buffer, needle))
        .expect("rendered text position is inside the buffer");
    (cell.fg, cell.bg, cell.modifier)
}

#[test]
fn the_leader_key_and_the_slash_command_both_open_the_panel_and_escape_closes_it() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);

    open_panel(&mut application);
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("Settings"),
        "leader+, opens the settings panel"
    );
    assert!(
        panel.contains("Left/Right tabs · Space change · Ctrl+D reset · Esc close"),
        "the footer teaches the keys the panel answers to, tab switching included: {panel}"
    );
    assert!(
        !panel.contains("Enter expand"),
        "and only the keys this tab answers to: nothing on General expands: {panel}"
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

/// The tab bar is the panel's map: every tab is always named, the active one
/// is drawn as such, and Left and Right walk between them in a ring so neither
/// end of the bar is a dead end.
#[test]
fn the_tab_bar_names_every_tab_and_left_and_right_switch_between_them_with_wrap() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    let opened = rendered_application_buffer(&application, 80, 15);
    let active = styling(&opened, "General");
    let idle = styling(&opened, "Providers");
    assert_ne!(
        active, idle,
        "the panel opens on General, drawn as the active tab"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Default Fold posture"),
        "and lists that tab's Settings"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    let switched = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        (
            styling(&switched, "Providers"),
            styling(&switched, "General")
        ),
        (active, idle),
        "Right moves the active tab on to Providers"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Codex"),
        "and the panel lists the Providers instead"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    let appearance = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&appearance, "Appearance"),
        active,
        "Right moves on again, to Appearance"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    let last = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&last, "Experimental"),
        active,
        "Right moves on again, to the last tab"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    let wrapped = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&wrapped, "General"),
        active,
        "Right past the last tab wraps to the first"
    );

    press(&mut application, KeyCode::Left, KeyModifiers::NONE);
    let backwards = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&backwards, "Experimental"),
        active,
        "Left before the first tab wraps to the last"
    );
}

#[test]
fn the_appearance_theme_row_opens_the_picker_and_cancel_returns_to_the_row() {
    let workspace = workspace_dir();
    let settings = EffectiveSettings {
        appearance: AppearanceSettings {
            theme: "catppuccin".to_owned(),
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    };
    let mut application = client_showing(workspace.path(), settings, &["appearance.theme"]);
    open_panel(&mut application);
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);

    let theme = row(&application, "Theme");
    assert!(
        theme.contains("catppuccin [pinned]"),
        "the row shows the effective open Setting value: {theme:?}"
    );
    assert_eq!(focused_key(&application), "appearance.theme");
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "opening a Theme picker is entirely client-local"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(" Themes ")
    );

    assert_eq!(
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert!(has_row(&application, "Theme"));
    assert_eq!(focused_key(&application), "appearance.theme");

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
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
        }),
        "the picker returns through the Theme row's typed pin"
    );
    assert!(has_row(&application, "Theme"));
    assert_eq!(focused_key(&application), "appearance.theme");
}

#[test]
fn space_cycles_appearance_mode_through_system_dark_and_light_then_wraps() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    focus_setting(&mut application, "appearance.mode");

    assert!(row(&application, "Mode").contains("system"));
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::Dark),
        })
    );

    let dark = EffectiveSettings {
        appearance: AppearanceSettings {
            mode: AppearanceMode::Dark,
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    };
    deliver_snapshot(&mut application, dark, &["appearance.mode"]);
    assert!(row(&application, "Mode").contains("dark [pinned]"));
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::Light),
        })
    );

    let light = EffectiveSettings {
        appearance: AppearanceSettings {
            mode: AppearanceMode::Light,
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    };
    deliver_snapshot(&mut application, light, &["appearance.mode"]);
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::System),
        }),
        "Space wraps past light to follow the terminal again"
    );
}

/// The Experimental tab is the last of the bar, past the Providers, and it is
/// the one surface an experimental Setting reaches the reader from: a Setting
/// declared experimental must not also stand among the General ones, and the
/// General tab must not lose its own.
#[test]
fn the_experimental_tab_stands_past_the_providers_and_lists_the_settings_declared_experimental() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_experimental_tab(&mut application);

    let emojis = row(&application, "Session name Emojis");
    assert!(
        emojis.contains("hidden [default]"),
        "a Session's name carries no Emoji until the reader asks for one: {emojis:?}"
    );
    assert_eq!(
        focused_key(&application),
        "session.title.emoji",
        "the focused Setting names the key a Config Document would spell"
    );
    assert!(
        !has_row(&application, "Default Fold posture") && !has_row(&application, "Codex"),
        "the Experimental tab lists its own Settings and nobody else's"
    );
    for label in ["Serving", "Serving port", "Serving bind address"] {
        assert!(
            has_row(&application, label),
            "the Experimental tab presents the {label} Server Setting"
        );
    }
    for key in ["serving.enabled", "serving.port", "serving.bindAddress"] {
        let descriptor = suru::settings::SCHEMA
            .iter()
            .find(|descriptor| descriptor.key == key)
            .unwrap_or_else(|| panic!("the schema does not declare {key}"));
        assert_eq!(descriptor.group, SettingGroup::Experimental);
        assert_eq!(descriptor.scope, SettingScope::Server);
    }

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionTitleEmoji {
            value: Some(EmojiVisibility::Shown),
        }),
        "and Space cycles it on, as it cycles any other Setting"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    assert!(
        !has_row(&application, "Session name Emojis"),
        "an experimental Setting is not also a General one"
    );
    assert!(
        has_row(&application, "Default Fold posture"),
        "and the General tab keeps every Setting it had"
    );
}

/// The tab bar is a surface the reader can point at, and pointing at a label
/// is the same arrival as walking onto it: the tab that presents the Providers
/// re-reads their Availability however the reader got there — the reader
/// already on it included, because clicking that label is the natural way to
/// ask Suru to look again after signing in outside it.
#[test]
fn clicking_a_tab_label_switches_to_that_tab() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    assert!(
        matches!(
            click_tab(&mut application, "Providers"),
            ApplicationTransition::ListModels(_)
        ),
        "clicking the Providers tab arrives on it and reads the Providers"
    );
    assert!(
        has_row(&application, "Codex"),
        "and the panel lists that tab's rows"
    );
    let switched = rendered_application_buffer(&application, 80, 15);
    assert_ne!(
        styling(&switched, "Providers"),
        styling(&switched, "General"),
        "with the tab bar drawing the clicked tab as the active one"
    );

    assert!(
        matches!(
            click_tab(&mut application, "Providers"),
            ApplicationTransition::ListModels(_)
        ),
        "clicking the tab already showing asks the Providers again"
    );

    assert_eq!(
        click_tab(&mut application, "General"),
        ApplicationTransition::Continue,
        "clicking back to General reads nothing"
    );
    assert!(
        has_row(&application, "Default Fold posture"),
        "and lists the Settings that configure no Provider"
    );
}

/// A click moves the focus and does nothing else: the row the reader pointed
/// at becomes the one an edit would act on, and the value it carries is left
/// exactly where it was.
#[test]
fn clicking_a_row_selects_it_without_changing_a_value() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    assert_eq!(
        focused_key(&application),
        "transcript.defaultFoldPosture",
        "the panel opens focused on its top row"
    );

    assert_eq!(
        click_row(&mut application, "Reasoning visibility"),
        ApplicationTransition::Continue,
        "a click selects, so nothing goes out to the Config Document"
    );
    assert_eq!(
        focused_key(&application),
        "transcript.reasoningVisibility",
        "the row the reader pointed at is the one an edit would now act on"
    );
    assert!(
        row(&application, "Reasoning visibility").contains("hidden [default]"),
        "and it is worth exactly what it was worth before the click"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::TranscriptReasoningVisibility {
            value: Some(ReasoningVisibility::Shown),
        }),
        "and Space still edits the focused Setting, wherever the focus came from"
    );
}

/// Every edit runs through the focused row, so a click on a Provider must not
/// be one: it may not turn the Provider off, and it may not stand in for the
/// Enter that reveals what the Provider carries.
#[test]
fn clicking_a_provider_row_neither_toggles_it_nor_expands_it() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);

    assert_eq!(
        click_row(&mut application, "Copilot"),
        ApplicationTransition::Continue,
        "no click ever edits a Setting"
    );
    assert_eq!(
        focused_key(&application),
        "provider.copilot.enabled",
        "the Provider row takes the focus like any other"
    );
    let copilot = row(&application, "Copilot");
    assert!(
        !copilot.contains("disabled"),
        "and the Provider is left exactly as it was: {copilot:?}"
    );

    // Back onto the Provider that carries further Settings, which is the one a
    // click could otherwise have opened.
    assert_eq!(
        click_row(&mut application, "Codex"),
        ApplicationTransition::Continue,
        "clicking an expandable Provider edits nothing either"
    );
    assert_eq!(
        focused_key(&application),
        "provider.codex.enabled",
        "it takes the focus"
    );
    assert!(
        !has_row(&application, "Reasoning summary"),
        "and stays collapsed, because expanding is Enter's and never the pointer's"
    );
}

/// The panel answers a click on a tab label and on a row, and on nothing else:
/// the headline, the footer, the tab bar's empty end, the border, and the
/// screen outside the box are all surfaces the reader may click through.
#[test]
fn a_click_on_anything_but_a_tab_or_a_row_changes_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    let buffer = rendered_application_buffer(&application, 80, 15);
    let (tabs_column, tabs_row) = text_position(&buffer, "Providers");
    let (headline_column, headline_row) = text_position(&buffer, "transcript.defaultFoldPosture");
    let (footer_column, footer_row) = text_position(&buffer, "Esc close");
    let (general_column, general_row) = text_position(&buffer, "General");
    let listed_row = row_index(
        &rendered_application_rows(&application),
        "Reasoning visibility",
    );
    let before = rendered_application_rows(&application);

    for (column, row) in [
        // Past the end of the last tab label, which is tab bar and nothing more.
        (tabs_column + "Providers".len() as u16 + 1, tabs_row),
        (headline_column, headline_row),
        (footer_column, footer_row),
        // The box's own left border, beside the tab bar and beside a row.
        (general_column - 1, general_row),
        (general_column - 1, listed_row as u16),
        // And the screen the overlay is drawn over.
        (0, 0),
    ] {
        assert_eq!(
            click(&mut application, column, row),
            ApplicationTransition::Continue,
            "a click at ({column}, {row}) asked something of Suru"
        );
        assert_eq!(
            rendered_application_rows(&application),
            before,
            "a click at ({column}, {row}) changed the panel"
        );
    }
}

#[test]
fn the_general_tab_lists_every_setting_that_configures_no_provider() {
    let workspace = workspace_dir();
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
    let visibility = row(&application, "Reasoning visibility");
    assert!(
        visibility.contains("hidden") && visibility.contains("[default]"),
        "a Setting nothing pins rides its built-in default: {visibility:?}"
    );
    assert_eq!(
        focused_key(&application),
        "transcript.defaultFoldPosture",
        "the focused Setting names the key a Config Document would spell"
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Codex"),
        "a Provider is the Providers tab's business, not General's"
    );
}

/// The Providers tab is the one place a reader manages Providers, so every
/// built-in Provider holds its place there whatever the reader has done to it:
/// a Provider turned off must stay findable to be turned back on.
#[test]
fn the_providers_tab_lists_every_built_in_provider_by_display_name_whatever_its_enablement() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        without_copilot(),
        &["provider.copilot.enabled"],
    );
    open_providers_tab(&mut application);

    let rows = rendered_application_rows(&application);
    assert!(
        row_index(&rows, "Codex") < row_index(&rows, "Copilot")
            && row_index(&rows, "Copilot") < row_index(&rows, "Claude"),
        "the Providers keep the built-in order, named as the reader knows them: {rows:?}"
    );

    let copilot = row(&application, "Copilot");
    assert!(
        copilot.contains("disabled") && copilot.contains("[pinned]"),
        "the Provider the reader turned off says so: {copilot:?}"
    );
    let claude = row(&application, "Claude");
    assert!(
        !claude.contains("disabled") && claude.contains("[default]"),
        "a Provider nothing pins is on, and an enabled Provider claims nothing further: {claude:?}"
    );

    let buffer = rendered_application_buffer(&application, 80, 15);
    assert_ne!(
        styling(&buffer, "Copilot"),
        styling(&buffer, "Claude"),
        "the disabled Provider's row is dimmed beside the Providers still on"
    );
}

#[test]
fn space_on_a_provider_row_toggles_that_providers_enablement() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");

    let headline = rendered_application_rows(&application).join("\n");
    assert!(
        headline.contains("Whether Suru offers Codex"),
        "the headline says what the row's one key would do: {headline}"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(false),
        }),
        "Space on a Provider row turns that Provider off"
    );
    assert!(
        !row(&application, "Codex").contains("disabled"),
        "the row waits for the server rather than showing an edit the file has not taken"
    );

    application
        .handle_event(ApplicationEvent::SettingMutated(snapshot(
            EffectiveSettings {
                provider: ProviderSettings {
                    codex: CodexSettings {
                        enabled: false,
                        ..CodexSettings::default()
                    },
                    ..ProviderSettings::default()
                },
                ..EffectiveSettings::default()
            },
            &["provider.codex.enabled"],
        )))
        .expect("receive the settings the edit left in force");
    let codex = row(&application, "Codex");
    assert!(
        codex.contains("disabled") && codex.contains("[pinned]"),
        "the row follows the refreshed snapshot the edit produced: {codex:?}"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled {
            value: Some(true),
        }),
        "Space turns a Provider back on"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexEnabled { value: None }),
        "and undoing the choice is as easy as making it"
    );
}

/// Availability is a fact about the environment that Suru re-reads when the
/// reader turns to a surface presenting the Providers themselves, so arriving
/// on the tab asks again — every time, however the reader got there — and
/// leaving asks nothing.
#[test]
fn entering_the_providers_tab_reads_availability_every_time() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);

    open_panel(&mut application);
    assert!(
        matches!(
            press(&mut application, KeyCode::Right, KeyModifiers::NONE),
            ApplicationTransition::ListModels(_)
        ),
        "arriving on the Providers tab reads the Providers"
    );
    assert_eq!(
        press(&mut application, KeyCode::Left, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "leaving it reads nothing"
    );
    assert!(
        matches!(
            press(&mut application, KeyCode::Right, KeyModifiers::NONE),
            ApplicationTransition::ListModels(_)
        ),
        "and coming back reads again rather than trusting what it heard before"
    );

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    open_panel(&mut application);
    assert_eq!(
        press(&mut application, KeyCode::Left, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "wrapping backwards off the first tab lands on one presenting no Provider"
    );
    assert!(
        matches!(
            press(&mut application, KeyCode::Left, KeyModifiers::NONE),
            ApplicationTransition::Continue
        ),
        "crossing Appearance reads no Provider"
    );
    assert!(
        matches!(
            press(&mut application, KeyCode::Left, KeyModifiers::NONE),
            ApplicationTransition::ListModels(_)
        ),
        "and reaching the tab from that side is the same arrival"
    );
}

/// A Provider Suru cannot use names the condition on its row and spells it out
/// in full where the reader is looking, because every way a Provider can be
/// unavailable is something they fix outside Suru.
#[test]
fn an_unavailable_provider_names_its_reason_and_the_headline_carries_the_message() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);

    deliver_catalog(
        &mut application,
        request,
        &[(
            "codex",
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotInstalled,
                message: "The codex CLI is not on PATH; install it and try again".to_owned(),
            },
        )],
    );

    let codex = row(&application, "Codex");
    assert!(
        codex.contains("not installed"),
        "the row names the reason: {codex:?}"
    );
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("install it and try again"),
        "and the headline carries the whole message while that row is selected: {panel}"
    );
}

/// A compatibility warning is guidance rather than Availability: the Provider
/// remains enabled, while its row names the warning and the focused headline
/// explains the action that clears it.
#[test]
fn a_provider_compatibility_warning_is_shown_without_disabling_the_provider() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);
    let message = "Codex CLI 0.149.0 may have compatibility issues; update to \
                   Codex CLI 0.150.1 or newer";

    deliver_catalog(
        &mut application,
        request,
        &[
            (
                "codex",
                ProviderCatalogStatus::Warning {
                    message: message.to_owned(),
                },
            ),
            ("copilot", ProviderCatalogStatus::Fresh),
            ("claude", ProviderCatalogStatus::Fresh),
        ],
    );

    let codex = row(&application, "Codex");
    assert!(
        codex.contains("warning"),
        "the row names the warning: {codex:?}"
    );
    assert!(
        !codex.contains("disabled") && !codex.contains("incompatible version"),
        "the warning does not claim the Provider is unavailable: {codex:?}"
    );
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("compatibility issues") && panel.contains("update"),
        "the focused headline explains the compatibility warning: {panel}"
    );
}

/// The row says only what the reader can act on: a read that failed is worth a
/// word, and a catalog that answered — however long ago — is worth none.
#[test]
fn a_failed_read_says_error_while_a_serving_catalog_says_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);

    deliver_catalog(
        &mut application,
        request,
        &[
            (
                "codex",
                ProviderCatalogStatus::Failed {
                    message: "the codex CLI answered with no Models at all".to_owned(),
                },
            ),
            ("copilot", ProviderCatalogStatus::Fresh),
            (
                "claude",
                ProviderCatalogStatus::Stale {
                    message: "the last refresh timed out".to_owned(),
                },
            ),
        ],
    );

    let codex = row(&application, "Codex");
    assert!(
        codex.contains("error"),
        "the Provider whose read failed says so: {codex:?}"
    );
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("answered with no Models"),
        "with the whole message in the headline: {panel}"
    );
    let copilot = row(&application, "Copilot");
    assert!(
        copilot.contains("Copilot [default]"),
        "a fresh catalog leaves nothing between the Provider and its provenance: {copilot:?}"
    );
    let claude = row(&application, "Claude");
    assert!(
        claude.contains("Claude [default]"),
        "and so does a stale one that is still serving what it last read: {claude:?}"
    );
}

/// Reading a Provider's Availability is live work, so it shows a Spinner while
/// it runs and nothing once the answer is in.
#[test]
fn a_provider_shows_a_spinner_while_its_availability_is_being_read() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);

    let reading = rendered_application_rows(&application);
    for provider in ["Codex", "Copilot", "Claude"] {
        assert!(
            reading[row_index(&reading, provider)].contains(SPINNER),
            "every enabled Provider is being read: {reading:?}"
        );
    }

    deliver_catalog(
        &mut application,
        request,
        &[
            ("codex", ProviderCatalogStatus::Fresh),
            ("copilot", ProviderCatalogStatus::Fresh),
            ("claude", ProviderCatalogStatus::Fresh),
        ],
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(SPINNER),
        "and the Spinner goes the moment the read settles"
    );
}

/// A Provider the reader turned off is one Suru leaves entirely alone, so its
/// row reports the choice and nothing Suru would have had to look for.
#[test]
fn a_disabled_provider_is_never_read_and_makes_no_availability_claim() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        without_codex(),
        &["provider.codex.enabled"],
    );
    let request = read_providers_tab(&mut application);

    let codex = row(&application, "Codex");
    assert!(
        codex.contains("disabled") && !codex.contains(SPINNER),
        "no read runs on its behalf, so nothing on its row is live: {codex:?}"
    );

    deliver_catalog(
        &mut application,
        request,
        &[(
            "codex",
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotSignedIn,
                message: "the codex CLI is not signed in".to_owned(),
            },
        )],
    );
    let codex = row(&application, "Codex");
    assert!(
        !codex.contains("not signed in") && !codex.contains("error"),
        "and its row claims nothing about a Provider Suru has not consulted: {codex:?}"
    );
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        !panel.contains("not signed in"),
        "the headline says nothing of it either: {panel}"
    );
}

/// The listing is the one thing asked of every Provider at once, so a listing
/// that never came back is every asked-about Provider's failed read.
#[test]
fn a_listing_that_never_came_back_leaves_every_asked_about_provider_in_error() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        without_codex(),
        &["provider.codex.enabled"],
    );
    let request = read_providers_tab(&mut application);

    application
        .handle_event(ApplicationEvent::ModelListingFailed {
            request,
            error: "the Suru server closed the connection mid-listing".to_owned(),
        })
        .expect("hear that the read never came back");

    for provider in ["Copilot", "Claude"] {
        let row = row(&application, provider);
        assert!(
            row.contains("error") && !row.contains(SPINNER),
            "the Provider asked about has its answer, such as it is: {row:?}"
        );
    }
    let codex = row(&application, "Codex");
    assert!(
        !codex.contains("error"),
        "and the one nothing was asked about is untouched by the failure: {codex:?}"
    );
    let disabled_headline = rendered_application_rows(&application).join("\n");
    assert!(
        !disabled_headline.contains("closed the connection"),
        "nor does the headline speak for it while its row is the focused one: {disabled_headline}"
    );

    // Down from the disabled Provider onto one the read did ask about.
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        panel.contains("closed the connection"),
        "the headline says what went wrong for the row the reader is on: {panel}"
    );
}

/// A read is a question this arrival asked, so only its own answer settles it:
/// a reader who stepped away and back has asked again, and the answer they left
/// behind must not stand in for the one they are waiting on.
#[test]
fn an_answer_to_a_superseded_read_settles_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let abandoned = read_providers_tab(&mut application);

    press(&mut application, KeyCode::Left, KeyModifiers::NONE);
    let ApplicationTransition::ListModels(awaited) =
        press(&mut application, KeyCode::Right, KeyModifiers::NONE)
    else {
        panic!("coming back asks again");
    };

    deliver_catalog(
        &mut application,
        abandoned,
        &[("codex", ProviderCatalogStatus::Fresh)],
    );
    assert!(
        row(&application, "Codex").contains(SPINNER),
        "the read in force is still out, whatever the abandoned one came back with"
    );

    deliver_catalog(
        &mut application,
        awaited,
        &[("codex", ProviderCatalogStatus::Fresh)],
    );
    assert!(
        !row(&application, "Codex").contains(SPINNER),
        "and its own answer is what settles it"
    );
}

/// A read that comes back saying nothing about a Provider has still come back:
/// the row reports that rather than waiting on an answer already given.
#[test]
fn a_provider_the_answer_passes_over_stops_waiting_and_says_so() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);

    deliver_catalog(
        &mut application,
        request,
        &[("codex", ProviderCatalogStatus::Fresh)],
    );
    let claude = row(&application, "Claude");
    assert!(
        claude.contains("error") && !claude.contains(SPINNER),
        "a Provider the answer never named is answered badly, not still being read: {claude:?}"
    );
}

/// Turning a Provider off takes back everything Suru had found out about it:
/// the row goes quiet, and so must the headline above it, which is the other
/// place a Provider speaks.
#[test]
fn turning_a_provider_off_after_a_read_silences_its_row_and_the_headline() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);
    deliver_catalog(
        &mut application,
        request,
        &[(
            "codex",
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotSignedIn,
                message: "the codex CLI is not signed in".to_owned(),
            },
        )],
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("not signed in"),
        "the Provider has something to say while it is on"
    );

    deliver_snapshot(
        &mut application,
        without_codex(),
        &["provider.codex.enabled"],
    );
    let panel = rendered_application_rows(&application).join("\n");
    assert!(
        !panel.contains("not signed in"),
        "and nothing at all once it is one Suru leaves entirely alone: {panel}"
    );
    let codex = row(&application, "Codex");
    assert!(
        codex.contains("disabled"),
        "which is all the row reports: {codex:?}"
    );
}

/// Enter is the Providers tab's one structural key: it reveals the Settings a
/// Provider carries beyond its Enablement, indented directly beneath it, and
/// hides them again.
#[test]
fn enter_reveals_a_providers_further_settings_beneath_it_and_hides_them_again() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    assert!(
        !has_row(&application, "Reasoning summary"),
        "a Provider's further Settings stay hidden until the reader asks for them"
    );
    let footer = rendered_application_rows(&application).join("\n");
    assert!(
        footer.contains("Enter expand"),
        "the row that expands teaches the key that expands it: {footer}"
    );

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    let rows = rendered_application_rows(&application);
    assert_eq!(
        row_index(&rows, "Reasoning summary"),
        row_index(&rows, "Codex") + 1,
        "the revealed Setting follows the Provider it configures: {rows:?}"
    );
    assert!(
        label_column(&application, "Reasoning summary") > label_column(&application, "Codex"),
        "drawn indented beneath it: {rows:?}"
    );
    let summary = row(&application, "Reasoning summary");
    assert!(
        summary.contains("auto") && summary.contains("[default]"),
        "and reading as an ordinary Setting row, value and provenance alike: {summary:?}"
    );

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !has_row(&application, "Reasoning summary"),
        "Enter again collapses the Provider"
    );
    assert!(
        has_row(&application, "Codex"),
        "leaving the Provider itself where it was"
    );
}

/// A revealed Setting is an ordinary row: the focus walks onto it, Space cycles
/// its value, Ctrl+D takes its pin out, and its provenance follows the
/// refreshed snapshot — the guard the flat panel carried before Providers had a
/// tab of their own.
#[test]
fn a_revealed_setting_navigates_cycles_and_resets_like_any_other_row() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        focused_key(&application),
        "provider.codex.reasoningSummary",
        "Down walks straight into the expansion, past no second Enablement row"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Concise),
        }),
        "Space cycles the revealed Setting on from the value in force"
    );

    deliver_snapshot(
        &mut application,
        EffectiveSettings {
            provider: ProviderSettings {
                codex: CodexSettings {
                    reasoning_summary: ReasoningSummaryDetail::Concise,
                    ..CodexSettings::default()
                },
                ..ProviderSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["provider.codex.reasoningSummary"],
    );
    let summary = row(&application, "Reasoning summary");
    assert!(
        summary.contains("concise") && summary.contains("[pinned]"),
        "the revealed row follows the refreshed snapshot the edit produced: {summary:?}"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: None,
        }),
        "and Ctrl+D unpins it as it would any other Setting"
    );

    deliver_snapshot(
        &mut application,
        EffectiveSettings {
            provider: ProviderSettings {
                codex: CodexSettings {
                    reasoning_summary: ReasoningSummaryDetail::None,
                    ..CodexSettings::default()
                },
                ..ProviderSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["provider.codex.reasoningSummary"],
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Auto),
        }),
        "a revealed Setting on its last value wraps to the first, like every other"
    );
}

/// The expansion is the Provider's own further Settings and nothing else: its
/// Enablement is what the Provider row already says, so repeating it inside
/// would give the reader two rows for one value.
#[test]
fn a_providers_enablement_never_appears_inside_its_own_expansion() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    let rows = rendered_application_rows(&application);
    let codex_rows = rows
        .iter()
        .filter(|row| is_row_for(row, "Codex Provider"))
        .count();
    assert_eq!(
        codex_rows, 0,
        "the Enablement's own label surfaces nowhere: {rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|row| is_row_for(row, "Codex")).count(),
        1,
        "and Codex holds exactly the one row that stands for it: {rows:?}"
    );
}

/// A Provider with nothing further to configure has nothing to expand, so it
/// offers no affordance and Enter on it does nothing at all.
#[test]
fn a_provider_with_no_further_settings_offers_no_expansion() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    assert!(
        row(&application, "Codex").contains('▸') && !row(&application, "Claude").contains('▸'),
        "the Provider that can be expanded says so, and the one that cannot stays quiet"
    );

    focus_setting(&mut application, "provider.claude.enabled");
    let before = rendered_application_rows(&application);
    assert!(
        !before.join("\n").contains("Enter expand"),
        "the panel does not teach a key this row has no answer to: {before:?}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "Enter on such a Provider emits nothing"
    );
    assert_eq!(
        rendered_application_rows(&application),
        before,
        "and changes nothing on screen"
    );
}

/// Expansion is view state the panel forgets: a reader returning to the panel
/// meets the same short list of Providers every time rather than whatever they
/// left open.
#[test]
fn every_provider_is_collapsed_each_time_the_panel_opens() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        has_row(&application, "Reasoning summary"),
        "the Provider is expanded"
    );

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    open_providers_tab(&mut application);
    assert!(
        !has_row(&application, "Reasoning summary"),
        "the reopened panel collapses every Provider"
    );
}

/// Enablement governs what Suru does with a Provider, not whether the reader
/// may configure it: a Provider turned off expands and its Settings take the
/// edits that apply when it is turned back on.
#[test]
fn a_disabled_provider_expands_and_its_settings_stay_editable() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        without_codex(),
        &["provider.codex.enabled"],
    );
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    assert!(
        row(&application, "Codex").contains("disabled"),
        "the Provider is one the reader turned off"
    );

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        has_row(&application, "Reasoning summary"),
        "which expands like any other"
    );
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Concise),
        }),
        "and whose Settings are edited as they would be were it enabled"
    );
}

/// Hopping to the Providers tab to turn something on and back is a round trip
/// a reader makes mid-edit, so neither tab may forget where they were.
#[test]
fn each_tab_keeps_its_own_selected_row_while_the_panel_is_open() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "transcript.reasoningVisibility");

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    focus_setting(&mut application, "provider.copilot.enabled");

    press(&mut application, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        focused_key(&application),
        "transcript.reasoningVisibility",
        "General is where the reader left it"
    );
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(
        focused_key(&application),
        "provider.copilot.enabled",
        "and so is Providers"
    );
}

#[test]
fn reopening_the_panel_starts_at_the_first_tab_and_its_top_row() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    focus_setting(&mut application, "provider.claude.enabled");
    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);

    open_panel(&mut application);
    assert_eq!(
        focused_key(&application),
        "transcript.defaultFoldPosture",
        "the panel opens on the first tab's top row, remembering nothing"
    );
    let reopened = rendered_application_buffer(&application, 80, 15);
    assert_ne!(
        styling(&reopened, "General"),
        styling(&reopened, "Providers"),
        "with General active again"
    );
}

#[test]
fn choosing_a_value_pins_it_and_the_row_follows_the_refreshed_snapshot() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    let transition = press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE);
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
fn session_content_width_row_spells_maxima_and_space_selects_fill() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");

    assert!(
        row(&application, "Session content width").contains("max 80 columns [default]"),
        "the built-in maximum is visible as a default"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Fill),
        }),
        "Space selects the Setting's named fill choice"
    );

    deliver_snapshot(
        &mut application,
        with_content_width(SessionContentWidth::Maximum(132)),
        &["session.contentWidth"],
    );
    assert!(
        row(&application, "Session content width").contains("max 132 columns [pinned]"),
        "an arbitrary maximum is spelled distinctly from its provenance"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('d'), KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth { value: None }),
        "the existing reset action removes the width pin"
    );
}

#[test]
fn session_content_width_opens_a_numeric_editor_prefilled_from_the_active_maximum() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        with_content_width(SessionContentWidth::Maximum(132)),
        &["session.contentWidth"],
    );
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");

    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "opening an editor is local presentation state"
    );
    let rendered = rendered_application_rows(&application).join("\n");
    assert!(
        rendered.contains("Maximum columns") && rendered.contains("132"),
        "the numeric editor opens over the panel with the active maximum prefilled: {rendered}"
    );
}

#[test]
fn a_valid_numeric_edit_emits_a_typed_mutation_and_waits_for_the_refreshed_snapshot() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        with_content_width(SessionContentWidth::Maximum(132)),
        &["session.contentWidth"],
    );
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut application, KeyCode::Char('2'), KeyModifiers::NONE);
    press(&mut application, KeyCode::Char('0'), KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Maximum(120)),
        }),
        "Enter applies the edited number through the Setting's typed mutation"
    );
    assert!(
        row(&application, "Session content width").contains("max 132 columns [pinned]"),
        "the row waits for the refreshed settings snapshot"
    );

    deliver_snapshot(
        &mut application,
        with_content_width(SessionContentWidth::Maximum(120)),
        &["session.contentWidth"],
    );
    assert!(
        row(&application, "Session content width").contains("max 120 columns [pinned]"),
        "the refreshed snapshot moves the row to the accepted maximum"
    );
}

/// One Setting holds both the threshold and the word that suspends it, the way
/// `session.contentWidth` holds both a maximum and `fill`: Space steps onto the
/// named value, and the numeric editor chooses any of the rest.
#[test]
fn the_auto_settle_row_spells_its_threshold_and_space_turns_settling_off() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "sidebar.autoSettle");

    assert!(
        row(&application, "Settle idle Sessions").contains("3 days [default]"),
        "the built-in threshold is visible as a default: {:?}",
        row(&application, "Settle idle Sessions")
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SidebarAutoSettle {
            value: Some(AutoSettle::Off),
        }),
        "Space selects the Setting's one named choice"
    );

    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "opening an editor is local presentation state"
    );
    let rendered = rendered_application_rows(&application).join("\n");
    assert!(
        rendered.contains("Days idle") && rendered.contains('3'),
        "the numeric editor opens prefilled with the threshold in force: {rendered}"
    );
    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut application, KeyCode::Char('7'), KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SidebarAutoSettle {
            value: Some(AutoSettle::Idle(7)),
        }),
        "Enter applies the edited number through the Setting's typed mutation"
    );
}

#[test]
fn invalid_numeric_input_stays_open_with_the_settings_validation_explanation() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    press(&mut application, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "incomplete input produces no mutation"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("minimum: 50"),
        "incomplete input keeps the editor open with its concise explanation"
    );

    press(&mut application, KeyCode::Char('4'), KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "a number below the minimum produces no mutation"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("minimum: 50"),
        "a number below the minimum remains editable with the same explanation"
    );
}

#[test]
fn fill_has_no_hidden_maximum_and_cancel_discards_the_numeric_edit() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        with_content_width(SessionContentWidth::Maximum(132)),
        &["session.contentWidth"],
    );
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Fill),
        })
    );
    deliver_snapshot(
        &mut application,
        with_content_width(SessionContentWidth::Fill),
        &["session.contentWidth"],
    );

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    let opened = rendered_application_rows(&application).join("\n");
    assert!(
        opened.contains("Maximum columns") && opened.contains("80"),
        "Fill opens from the Setting's useful default rather than hidden state: {opened}"
    );
    press(&mut application, KeyCode::Char('9'), KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "cancelling produces no mutation"
    );
    assert!(
        row(&application, "Session content width").contains("fill [pinned]"),
        "cancelling returns to the unchanged panel"
    );

    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    let reopened = rendered_application_rows(&application).join("\n");
    assert!(
        reopened.contains("Maximum columns") && reopened.contains("80"),
        "reopening from Fill forgets the cancelled number: {reopened}"
    );
}

#[test]
fn numeric_editor_commands_can_be_invoked_semantically() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    for command in [
        SemanticCommandId::SettingsNumericDeleteBackward,
        SemanticCommandId::SettingsNumericDeleteBackward,
        SemanticCommandId::SettingsNumericInsert(NumericDigit::Five),
        SemanticCommandId::SettingsNumericInsert(NumericDigit::Zero),
    ] {
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    command,
                )))
                .expect("invoke a numeric editor command"),
            ApplicationTransition::Continue
        );
    }
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SettingsNumericApply,
            )))
            .expect("apply a numeric edit semantically"),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Maximum(50)),
        }),
        "plugins and future pointer behavior reach the same typed mutation as the keyboard"
    );
}

#[test]
fn closing_the_settings_panel_also_closes_its_numeric_editor() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SettingsClose,
        )))
        .expect("close the settings panel through its semantic command");
    let closed = rendered_application_rows(&application).join("\n");
    assert!(
        !closed.contains("Maximum columns") && !closed.contains(" Settings "),
        "closing the owner leaves no invisible numeric editor behind: {closed}"
    );

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SettingsNumericInsert(NumericDigit::Nine),
            )))
            .expect("invoke stale editor input after the panel closed"),
        ApplicationTransition::Continue,
        "input aimed at the closed editor changes nothing"
    );
}

#[test]
fn a_numeric_editor_hidden_by_a_newer_overlay_accepts_no_input() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.contentWidth");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    assert!(
        matches!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    SemanticCommandId::ModelList,
                )))
                .expect("open the Model picker over the numeric editor"),
            ApplicationTransition::ListModels(_)
        ),
        "the newer overlay opens"
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SettingsNumericInsert(NumericDigit::Nine),
        )))
        .expect("aim numeric input at the hidden editor");
    application
        .handle_event(ApplicationEvent::Command(CommandId::CloseModelPicker))
        .expect("close the newer overlay");

    for command in [
        SemanticCommandId::SettingsNumericDeleteBackward,
        SemanticCommandId::SettingsNumericDeleteBackward,
        SemanticCommandId::SettingsNumericInsert(NumericDigit::Five),
        SemanticCommandId::SettingsNumericInsert(NumericDigit::Zero),
    ] {
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                command,
            )))
            .expect("edit the visible numeric editor");
    }
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SettingsNumericApply,
            )))
            .expect("apply after the newer overlay closes"),
        ApplicationTransition::MutateSetting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Maximum(50)),
        }),
        "the editor did not accept input while another overlay hid it"
    );
}

/// Space is the one change key, and it only ever steps forward: a Setting
/// holding its last value wraps to the first rather than stopping.
#[test]
fn space_cycles_a_setting_forward_and_wraps_past_the_last_value() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        EffectiveSettings {
            transcript: TranscriptSettings {
                reasoning_visibility: ReasoningVisibility::Shown,
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["transcript.reasoningVisibility"],
    );
    open_panel(&mut application);
    focus_setting(&mut application, "transcript.reasoningVisibility");

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::TranscriptReasoningVisibility {
            value: Some(ReasoningVisibility::Hidden),
        }),
        "stepping on from the last value wraps to the first"
    );
}

#[test]
fn the_sidebar_scope_row_cycles_through_all_workspaces_current_workspace_and_everywhere() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "sidebar.initialScope");

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SidebarInitialScope {
            value: Some(SidebarScope::CurrentWorkspace),
        })
    );
    deliver_snapshot(
        &mut application,
        with_sidebar_scope(SidebarScope::CurrentWorkspace),
        &["sidebar.initialScope"],
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SidebarInitialScope {
            value: Some(SidebarScope::Everywhere),
        })
    );
    deliver_snapshot(
        &mut application,
        with_sidebar_scope(SidebarScope::Everywhere),
        &["sidebar.initialScope"],
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SidebarInitialScope {
            value: Some(SidebarScope::AllWorkspaces),
        }),
        "the third value wraps to the first"
    );
}

/// Enter opens a row onto what it stands for and never edits, so on a row
/// standing for nothing to open — a General Setting cycled through words, or a
/// Setting revealed inside an expansion — it changes neither the shape of the
/// panel nor any value.
#[test]
fn enter_never_edits_a_value_and_opens_nothing_on_a_row_that_stands_for_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "Enter edited a General Setting"
    );
    assert!(
        row(&application, "Default Fold posture").contains("folded [default]"),
        "every Setting stays exactly where it was"
    );

    application
        .handle_event(ApplicationEvent::SettingMutationFailed(
            "Config Document \"suru.jsonc\" is not valid JSONC".to_owned(),
        ))
        .expect("hear that an edit never landed");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("not valid JSONC"),
        "a key that does nothing takes nothing away, the standing complaint included"
    );

    press(&mut application, KeyCode::Right, KeyModifiers::NONE);
    focus_setting(&mut application, "provider.codex.enabled");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    let expanded = rendered_application_rows(&application);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "Enter edited a revealed Setting"
    );
    assert_eq!(
        rendered_application_rows(&application),
        expanded,
        "a revealed Setting has nothing of its own to expand"
    );
}

#[test]
fn resetting_a_pinned_setting_unpins_it_and_the_row_returns_to_the_default() {
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let request = read_providers_tab(&mut application);
    // The row the refusal is about is also one with something of its own to
    // say, so the complaint has to outrank it.
    deliver_catalog(
        &mut application,
        request,
        &[(
            "codex",
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotInstalled,
                message: "the codex CLI is not on PATH".to_owned(),
            },
        )],
    );
    press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE);

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
        !row(&application, "Codex").contains("disabled"),
        "a refused edit leaves the Provider exactly where it was"
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
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);

    for command in [
        SemanticCommandId::SettingsValueCycle,
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

/// A Setting revealed inside an expansion is an ordinary row, so the pointer
/// reaches it as it reaches any other — and no more than any other: the value
/// stays where it was until the reader edits it from the keyboard.
#[test]
fn clicking_a_revealed_setting_focuses_it_like_any_other_row() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.codex.enabled");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        click_row(&mut application, "Reasoning summary"),
        ApplicationTransition::Continue,
        "a revealed Setting is selected by a click, not edited by one"
    );
    assert_eq!(
        focused_key(&application),
        "provider.codex.reasoningSummary",
        "the revealed row the reader pointed at is the focused one"
    );
    assert!(
        row(&application, "Reasoning summary").contains("auto [default]"),
        "and it is worth exactly what it was worth before the click"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Concise),
        }),
        "Space edits the row the click focused, the edit gate intact"
    );
}

/// A box too short for its tab holds a window onto the rows rather than all of
/// them, so the row under the pointer is the one that window drew there and not
/// the tab's row of the same number.
#[test]
fn a_click_lands_on_the_row_the_window_drew_there() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_providers_tab(&mut application);
    focus_setting(&mut application, "provider.claude.enabled");

    // Short enough that the box has room for the last rows of the tab alone.
    let cramped = rendered_application_rows_at(&application, 80, 9);
    assert!(
        !cramped.iter().any(|row| is_row_for(row, "Codex")),
        "the window has scrolled past the tab's first row: {cramped:?}"
    );

    click(
        &mut application,
        label_column_in(&cramped, "Copilot") as u16,
        row_index(&cramped, "Copilot") as u16,
    );
    assert_eq!(
        focused_key(&application),
        "provider.copilot.enabled",
        "the click landed on the row drawn there rather than on the tab's first"
    );
}

/// A pointer lands on what is on screen, so a frame that drew no panel leaves
/// nothing to land on: a terminal too small for the panel answers a click with
/// nothing rather than with where the panel last stood.
#[test]
fn a_click_at_a_terminal_too_small_to_draw_the_panel_changes_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    let rows = rendered_application_rows(&application);
    let listed_row = row_index(&rows, "Reasoning visibility") as u16;
    let column = label_column(&application, "Reasoning visibility") as u16;

    let cramped = rendered_application_rows_at(&application, 20, 4);
    assert!(
        !cramped.join("\n").contains("Reasoning visibility"),
        "the terminal is too small to draw the panel at all: {cramped:?}"
    );

    assert_eq!(
        click(&mut application, column, listed_row),
        ApplicationTransition::Continue,
        "a click asked something of a panel this frame never drew"
    );
    assert_eq!(
        focused_key(&application),
        "transcript.defaultFoldPosture",
        "the focus is where the reader left it, not where the panel used to be"
    );
}

/// The pointer reaches the panel as a command like any other, so the command
/// can arrive while the panel is closed. It must move no focus the reader
/// cannot see and send Suru off to consult no Provider.
#[test]
fn a_pointer_command_invoked_while_the_panel_is_closed_changes_nothing() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    let before = rendered_application_rows(&application);

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::FocusSettingsPanelAt {
                column: 4,
                screen_row: 6,
            }))
            .expect("point at a panel that is not open"),
        ApplicationTransition::Continue,
        "a click with no panel open asked something of Suru"
    );
    assert_eq!(
        rendered_application_rows(&application),
        before,
        "and changed what is on screen"
    );
}

#[test]
fn the_open_panel_takes_the_keys_the_composer_would_otherwise_get() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);

    // Space-free, because Space is the panel's own change key rather than a
    // character the composer would have taken.
    type_terminal_text(&mut application, "not-a-Prompt");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("not-a-Prompt"),
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

/// Effective settings whose only departure from the built-in defaults is which
/// Agent Selection derives a Session's Title.
fn deriving_titles_with(errand: TitleErrand) -> EffectiveSettings {
    EffectiveSettings {
        session: SessionSettings {
            title: TitleSettings {
                errand,
                ..TitleSettings::default()
            },
            ..SessionSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// The Agent Selection a reader would pin, and the Model the picker offers to
/// pin it with.
fn pinned_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5-mini"),
        options: Vec::new(),
    }
}

/// Two Models, so a test can tell the one the picker focuses from the one it
/// would have focused on its own: the second is the Provider's default.
fn two_model_catalog() -> ModelCatalog {
    // The cheap Model carries a Model Option, which is what makes the pin's own
    // Options worth asserting: a Model with none could not tell "the Model the
    // reader chose" from "that Model with today's defaults baked in".
    let mut cheap = model_descriptor(
        "codex",
        "gpt-5-mini",
        "GPT-5 Mini",
        false,
        ModelAvailability::Available,
    );
    cheap.options = vec![ModelOptionDescriptor {
        id: ModelOptionId::new("reasoning_effort"),
        label: "Reasoning".to_owned(),
        description: None,
        role: ModelOptionRole::ReasoningEffort,
        kind: ModelOptionKind::Select {
            choices: vec![ModelOptionChoice {
                id: ModelOptionChoiceId::new("low"),
                label: "Low".to_owned(),
                description: None,
                availability: ModelAvailability::Available,
            }],
            default: ModelOptionChoiceId::new("low"),
        },
    }];
    ModelCatalog {
        providers: vec![ProviderModelCatalog {
            provider: ProviderId::new("codex"),
            display_name: "Codex".to_owned(),
            models: vec![
                cheap,
                model_descriptor(
                    "codex",
                    "gpt-5",
                    "GPT-5",
                    true,
                    ModelAvailability::Available,
                ),
            ],
            status: ProviderCatalogStatus::Fresh,
        }],
    }
}

/// The Model row the picker has focused, which is the row it marks.
fn focused_model(application: &Application) -> String {
    let rows = rendered_application_rows(application);
    rows.iter()
        .find(|row| row.contains('›'))
        .unwrap_or_else(|| panic!("the Model picker focused nothing: {rows:?}"))
        .trim()
        .to_owned()
}

/// Title derivation is the first Setting the panel cannot present as a ring of
/// words: two of its values are words, and the third is an Agent Selection the
/// reader chooses at the Model picker rather than types.
#[test]
fn the_title_derivation_row_cycles_its_named_values_and_spells_a_pinned_selection() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.title.errand");

    assert!(
        row(&application, "Title derivation").contains("· session"),
        "the built-in default follows the Session's own Provider: {:?}",
        row(&application, "Title derivation")
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Enter choose · "),
        "the focused row teaches the key that opens the surface its value is chosen at"
    );

    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::Off),
        }),
        "Space walks the values the schema does name"
    );

    deliver_snapshot(
        &mut application,
        deriving_titles_with(TitleErrand::Pinned(pinned_selection())),
        &["session.title.errand"],
    );
    let pinned_row = row(&application, "Title derivation");
    assert!(
        pinned_row.contains("codex · gpt-5-mini") && pinned_row.contains("[pinned]"),
        "a value the schema never named is spelled by the Setting itself: {pinned_row:?}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char(' '), KeyModifiers::NONE),
        ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::FollowSession),
        }),
        "and cycling off it returns to the first value that has a word"
    );
}

#[test]
fn opening_the_title_derivation_row_pins_the_model_chosen_at_the_picker() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.title.errand");

    let ApplicationTransition::ListModels(request) =
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("opening the row opens the Model picker onto a fresh catalog listing");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: two_model_catalog(),
        })
        .expect("receive the catalog the picker asked for");
    let showing = rendered_application_rows(&application).join("\n");
    assert!(
        showing.contains("GPT-5 Mini") && !showing.contains("Title derivation"),
        "the picker is drawn over the panel that opened it, and answers the keys: {showing}"
    );
    assert!(
        focused_model(&application).contains("· gpt-5 ·"),
        "a Setting holding no Selection yet opens the picker where it would open anyway: {:?}",
        focused_model(&application)
    );

    // Off the Provider's default and onto the cheap Model, which is the whole
    // point of pinning one for Title derivation.
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "choosing a configurable Model stages its options before saving"
    );
    let options = rendered_application_rows(&application).join("\n");
    assert!(options.contains("Model Options"), "{options}");
    assert!(
        options.contains("Reasoning · Provider default"),
        "{options}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::Pinned(pinned_selection())),
        }),
        "the Model the reader chose is pinned as this Setting's value, not as the Agent, \
         and the pin claims the Model alone rather than freezing today's Model Options"
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("GPT-5 Mini"),
        "and the picker closes, leaving the reader back on the row they opened"
    );

    // The server answers the edit, and reopening the row lands on the choice
    // the reader made rather than back on the Provider's default.
    deliver_snapshot(
        &mut application,
        deriving_titles_with(TitleErrand::Pinned(pinned_selection())),
        &["session.title.errand"],
    );
    let ApplicationTransition::ListModels(reopened) =
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("reopening the row reads the catalog again");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: reopened,
            catalog: two_model_catalog(),
        })
        .expect("receive the catalog the reopened picker asked for");
    assert!(
        focused_model(&application).contains("gpt-5-mini"),
        "the picker opens on the Model this Setting already holds: {:?}",
        focused_model(&application)
    );
}

/// The Session's own Model is not what this picker is asking about. The
/// listing the row's picker asks for lands after it opens, and every other
/// picker in Suru focuses the Session's Selection when one does — so this is
/// the frame where a Setting's pin is easiest to lose.
#[test]
fn a_catalog_landing_leaves_the_pinned_model_focused_rather_than_the_sessions() {
    let workspace = workspace_dir();
    let mut application = client_showing(
        workspace.path(),
        deriving_titles_with(TitleErrand::Pinned(pinned_selection())),
        &["session.title.errand"],
    );
    // A Session conversing at the Provider's default Model, which is the Model
    // this picker must not be dragged onto.
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(
                SessionId::new(),
                workspace.path(),
                AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-5"),
                    options: Vec::new(),
                },
            ),
        ))
        .expect("attach a Session conversing at another Model");
    open_panel(&mut application);
    focus_setting(&mut application, "session.title.errand");

    let ApplicationTransition::ListModels(request) =
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("opening the row opens the Model picker");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: two_model_catalog(),
        })
        .expect("receive the catalog the picker asked for");

    assert!(
        focused_model(&application).contains("gpt-5-mini"),
        "the catalog landing moved the focus onto the Session's Model rather than the pin: {:?}",
        focused_model(&application)
    );
}

/// Cancelling is the other way out of the picker, and it must leave the panel
/// exactly as the reader left it rather than closing it along with the picker.
#[test]
fn cancelling_the_picker_leaves_the_settings_panel_where_it_was() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_panel(&mut application);
    focus_setting(&mut application, "session.title.errand");
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);

    assert!(
        has_row(&application, "Title derivation"),
        "Esc closes the picker alone"
    );
    assert_eq!(
        focused_key(&application),
        "session.title.errand",
        "and the panel is still focused on the row that opened it"
    );
}

fn open_title_options(application: &mut Application, catalog: ModelCatalog) {
    open_panel(application);
    focus_setting(application, "session.title.errand");
    let ApplicationTransition::ListModels(request) =
        press(application, KeyCode::Enter, KeyModifiers::NONE)
    else {
        panic!("title selection should request Models");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed { request, catalog })
        .expect("load title Models");
    assert_eq!(
        press(application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(application)
            .join("\n")
            .contains("Model Options")
    );
}

fn configurable_title_catalog(provider: &str) -> ModelCatalog {
    let mut catalog = two_model_catalog();
    let entry = &mut catalog.providers[0];
    entry.provider = ProviderId::new(provider);
    entry.display_name = provider.to_owned();
    entry.models.truncate(1);
    let model = &mut entry.models[0];
    model.provider = ProviderId::new(provider);
    model.is_default = true;
    model.options.push(ModelOptionDescriptor {
        id: ModelOptionId::new("fast"),
        label: "Speed".to_owned(),
        description: None,
        role: ModelOptionRole::Speed,
        kind: ModelOptionKind::Toggle { default: false },
    });
    catalog
}

#[test]
fn title_options_pin_explicit_choices_even_equal_to_defaults_across_providers() {
    for provider in ["codex", "copilot", "claude"] {
        let workspace = workspace_dir();
        let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
        open_title_options(&mut application, configurable_title_catalog(provider));
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
        let choices = rendered_application_rows(&application).join("\n");
        assert!(choices.contains("Provider default [current]"), "{choices}");
        press(&mut application, KeyCode::Down, KeyModifiers::NONE);
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
        let mut expected = pinned_selection();
        expected.provider = ProviderId::new(provider);
        expected.options.push(ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("low"),
            },
        });
        assert_eq!(
            press(&mut application, KeyCode::Enter, KeyModifiers::CONTROL),
            ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
                value: Some(TitleErrand::Pinned(expected)),
            }),
            "explicit Low is pinned while untouched Speed follows defaults for {provider}"
        );
    }
}

#[test]
fn title_options_reopen_own_overrides_and_can_restore_provider_defaults() {
    let workspace = workspace_dir();
    let mut selection = pinned_selection();
    selection.options.push(ModelOptionSelection {
        id: ModelOptionId::new("fast"),
        value: ModelOptionValue::Toggle { enabled: true },
    });
    let mut application = client_showing(
        workspace.path(),
        deriving_titles_with(TitleErrand::Pinned(selection)),
        &["session.title.errand"],
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(
                SessionId::new(),
                workspace.path(),
                AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-5"),
                    options: Vec::new(),
                },
            ),
        ))
        .expect("attach a Session with a different Agent Selection");
    open_title_options(&mut application, configurable_title_catalog("codex"));
    let options = rendered_application_rows(&application).join("\n");
    assert!(options.contains("Speed · On"), "{options}");
    assert!(
        options.contains("Reasoning · Provider default"),
        "{options}"
    );
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    // On -> Provider default wraps past the last explicit choice.
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::CONTROL),
        ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::Pinned(pinned_selection())),
        })
    );
}

#[test]
fn cancelling_title_options_discards_staged_changes_and_preserves_the_setting() {
    let workspace = workspace_dir();
    let mut application = client_showing(workspace.path(), EffectiveSettings::default(), &[]);
    open_title_options(&mut application, configurable_title_catalog("codex"));
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Speed · On")
    );
    assert_eq!(
        press(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert_eq!(focused_key(&application), "session.title.errand");
    assert!(row(&application, "Title derivation").contains("· session"));
}

#[test]
fn choosing_another_title_model_or_provider_starts_with_inherited_options() {
    for (provider, model) in [("codex", "another-model"), ("claude", "gpt-5-mini")] {
        let workspace = workspace_dir();
        let mut original = pinned_selection();
        original.options.push(ModelOptionSelection {
            id: ModelOptionId::new("fast"),
            value: ModelOptionValue::Toggle { enabled: true },
        });
        let mut application = client_showing(
            workspace.path(),
            deriving_titles_with(TitleErrand::Pinned(original)),
            &["session.title.errand"],
        );
        let mut catalog = configurable_title_catalog(provider);
        catalog.providers[0].models[0].id = ModelId::new(model);
        open_title_options(&mut application, catalog);
        assert!(
            rendered_application_rows(&application)
                .join("\n")
                .contains("Speed · Provider default")
        );
        assert_eq!(
            press(&mut application, KeyCode::Enter, KeyModifiers::CONTROL),
            ApplicationTransition::MutateSetting(SettingMutation::SessionTitleErrand {
                value: Some(TitleErrand::Pinned(AgentSelection {
                    provider: ProviderId::new(provider),
                    model: ModelId::new(model),
                    options: Vec::new(),
                })),
            })
        );
    }
}
