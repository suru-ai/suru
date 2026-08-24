//! The settings panel: its General and Providers tabs, and editing a Setting
//! from either of them.

use std::path::Path;

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};

use crate::support::{
    connected_application, rendered_application_buffer, rendered_application_rows, text_position,
    type_terminal_text,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        CodexSettings, CopilotSettings, EffectiveSettings, FoldPosture, ModelCatalog,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, ProviderSettings,
        ProviderUnavailability, ReasoningSummaryDetail, ReasoningVisibility, SettingMutation,
        SettingsSnapshot, TranscriptSettings,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, ModelListRequest,
        SemanticCommandId,
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

/// The panel as it opens, on the Providers tab.
fn open_providers_tab(application: &mut Application) {
    open_panel(application);
    press(application, KeyCode::Right, KeyModifiers::NONE);
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
    let rows = rendered_application_rows(application);
    let index = row_index(&rows, label);
    let row = &rows[index];
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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

/// The tab bar is the panel's map: both tabs are always named, the active one
/// is drawn as such, and Left and Right walk between them in a ring so neither
/// end of the bar is a dead end.
#[test]
fn the_tab_bar_names_both_tabs_and_left_and_right_switch_between_them_with_wrap() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let wrapped = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&wrapped, "General"),
        active,
        "Right past the last tab wraps to the first"
    );

    press(&mut application, KeyCode::Left, KeyModifiers::NONE);
    let backwards = rendered_application_buffer(&application, 80, 15);
    assert_eq!(
        styling(&backwards, "Providers"),
        active,
        "Left before the first tab wraps to the last"
    );
}

#[test]
fn the_general_tab_lists_every_setting_that_configures_no_provider() {
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    assert!(
        matches!(
            press(&mut application, KeyCode::Left, KeyModifiers::NONE),
            ApplicationTransition::ListModels(_)
        ),
        "reaching the tab by wrapping the other way is the same arrival"
    );
}

/// A Provider Suru cannot use names the condition on its row and spells it out
/// in full where the reader is looking, because every way a Provider can be
/// unavailable is something they fix outside Suru.
#[test]
fn an_unavailable_provider_names_its_reason_and_the_headline_carries_the_message() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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

/// The row says only what the reader can act on: a read that failed is worth a
/// word, and a catalog that answered — however long ago — is worth none.
#[test]
fn a_failed_read_says_error_while_a_serving_catalog_says_nothing() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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

/// Space is the one change key, and it only ever steps forward: a Setting
/// holding its last value wraps to the first rather than stopping.
#[test]
fn space_cycles_a_setting_forward_and_wraps_past_the_last_value() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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

/// Enter means expand and collapse and nothing else, so on a row that is not a
/// Provider — a General Setting, or a Setting revealed inside an expansion —
/// it changes neither the shape of the panel nor any value.
#[test]
fn enter_is_a_no_op_on_every_row_that_is_not_a_provider() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    let workspace = tempfile::tempdir().expect("create Workspace");
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

#[test]
fn the_open_panel_takes_the_keys_the_composer_would_otherwise_get() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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
