//! Copy-only selections across the Application's prose surfaces.
use super::support::*;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Modifier;
use suru::{
    protocol::{EffectiveSettings, TextSelectionCopy},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

fn release_copy(application: &mut Application) {
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = TextSelectionCopy::Release;
    settings.sidebar.initial_visibility = suru::protocol::SidebarVisibility::Hidden;
    deliver_settings(application, settings);
}

pub(super) fn mouse(
    application: &mut Application,
    kind: MouseEventKind,
    position: (u16, u16),
) -> ApplicationTransition {
    application
        .handle_terminal_event(Event::Mouse(MouseEvent {
            kind,
            column: position.0,
            row: position.1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap()
}

pub(super) fn drag(
    application: &mut Application,
    start: (u16, u16),
    end: (u16, u16),
) -> ApplicationTransition {
    mouse(application, MouseEventKind::Down(MouseButton::Left), start);
    mouse(application, MouseEventKind::Drag(MouseButton::Left), end);
    mouse(application, MouseEventKind::Up(MouseButton::Left), end)
}

#[test]
fn settings_prose_copies_while_tabs_and_outside_drags_do_not_select() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SettingsOpen,
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 100, 32);
    let start = text_position(&buffer, "Copy Text Selection");
    let end = (start.0 + 18, start.1);
    let transition = drag(&mut application, start, end);
    assert_eq!(
        transition,
        ApplicationTransition::CopyToClipboard("Copy Text Selection".into())
    );
    let selected = rendered_application_buffer(&application, 100, 32);
    assert!(selected[start].modifier.contains(Modifier::REVERSED));
    application
        .handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .unwrap();
    assert!(
        rendered_application_rows_at(&application, 100, 32)
            .join("\n")
            .contains("Copy Text Selection")
    );
    let tab = text_position(&buffer, "General");
    assert_eq!(
        drag(&mut application, tab, (tab.0 + 3, tab.1)),
        ApplicationTransition::Continue
    );
    let buffer = rendered_application_buffer(&application, 100, 32);
    assert!(!buffer[tab].modifier.contains(Modifier::REVERSED));
    mouse(
        &mut application,
        MouseEventKind::Down(MouseButton::Left),
        (99, 31),
    );
    mouse(
        &mut application,
        MouseEventKind::Drag(MouseButton::Left),
        start,
    );
    let buffer = rendered_application_buffer(&application, 100, 32);
    assert!(!buffer[start].modifier.contains(Modifier::REVERSED));
    mouse(
        &mut application,
        MouseEventKind::Up(MouseButton::Left),
        start,
    );
    assert!(
        !rendered_application_rows_at(&application, 100, 32)
            .join("\n")
            .contains("Copy Text Selection")
    );
}

#[test]
fn pickers_and_connection_overlays_copy_their_painted_rows() {
    for (command, needle) in [
        (SemanticCommandId::ThemeList, "Type to search"),
        (SemanticCommandId::ModelList, "Loading Models"),
        (SemanticCommandId::SessionList, "Loading Sessions"),
        (SemanticCommandId::WorkspaceList, "Loading Workspaces"),
        (SemanticCommandId::ServeOpen, "Preparing Serving"),
        (SemanticCommandId::ConnectOpen, "Loading Remotes"),
        (SemanticCommandId::RelayOpen, "Loading Relays"),
    ] {
        let workspace = workspace_dir();
        let mut application = connected_application(workspace.path());
        release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                command,
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 100, 32);
        let start = text_position(&buffer, needle);
        let end = (start.0 + needle.len() as u16 - 1, start.1);
        assert_eq!(
            drag(&mut application, start, end),
            ApplicationTransition::CopyToClipboard(needle.into()),
            "{command:?}"
        );
        assert!(
            rendered_application_buffer(&application, 100, 32)[start]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            drag(&mut application, (99, 31), start),
            ApplicationTransition::Continue
        );
        assert!(
            !rendered_application_rows_at(&application, 100, 32)
                .join("\n")
                .contains(needle),
            "{command:?}"
        );
    }
}

/// What fixes an unavailable Provider, wrapped beneath its retry row, copies
/// as it was written: the wrap that broke an install page's address across
/// rows, and the indent that set it beneath the row, are the picker's layout
/// rather than the text.
#[test]
fn a_wrapped_fix_in_the_model_picker_copies_as_one_unbroken_line() {
    use suru::protocol::*;
    const INSTALL_PAGE: &str =
        "https://docs.github.com/en/copilot/how-tos/set-up/install-copilot-cli";
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    release_copy(&mut application);
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .unwrap()
    else {
        panic!("the picker requests Models")
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("copilot"),
                    display_name: "Copilot".to_owned(),
                    models: Vec::new(),
                    status: ProviderCatalogStatus::Unavailable {
                        reason: ProviderUnavailability::NotInstalled,
                        message: "could not launch `copilot`".to_owned(),
                        fix: format!("install the Copilot CLI from {INSTALL_PAGE}"),
                    },
                }],
            },
        })
        .unwrap();

    let buffer = rendered_application_buffer(&application, 60, 24);
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
    let highlighted = rendered_application_buffer(&application, 60, 24);
    for cell in [start, address, end] {
        assert!(
            highlighted[cell].modifier.contains(Modifier::REVERSED),
            "the fix's text lights up where it is drawn, at {cell:?}"
        );
    }
    assert!(
        !highlighted[(address.0 - 1, address.1)]
            .modifier
            .contains(Modifier::REVERSED),
        "while the indent it is drawn beneath the retry row from does not"
    );
    assert_eq!(
        drag(&mut application, address, end),
        ApplicationTransition::CopyToClipboard(INSTALL_PAGE.into()),
        "and the address alone copies unbroken"
    );
}

#[test]
fn composer_and_transcript_drags_stay_on_the_surface_they_started_on() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    release_copy(&mut application);
    enter_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "draft remains here");
    let buffer = rendered_application_buffer(&application, 100, 32);
    let composer = text_position(&buffer, "draft remains here");
    let transcript = text_position(&buffer, "Initial Prompt");
    assert_eq!(
        drag(&mut application, composer, (99, transcript.1)),
        ApplicationTransition::CopyToClipboard("draft remains here".into())
    );
    let buffer = rendered_application_buffer(&application, 100, 32);
    assert!(!buffer[transcript].modifier.contains(Modifier::REVERSED));
    let copied = drag(&mut application, transcript, (99, composer.1));
    assert!(
        matches!(copied, ApplicationTransition::CopyToClipboard(ref text) if text.text.contains("Initial Prompt") && !text.text.contains("draft remains here")),
        "{copied:?}"
    );
    assert!(
        !rendered_application_buffer(&application, 100, 32)[composer]
            .modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn model_options_copy_multiple_painted_rows_and_manual_copy_clears_the_highlight() {
    use suru::protocol::*;
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = TextSelectionCopy::Manual;
    deliver_settings(&mut application, settings);
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptions,
        )))
        .unwrap()
    else {
        panic!("options requests Models")
    };
    let mut model = model_descriptor(
        "codex",
        "sample",
        "Sample 界🙂 Model",
        true,
        ModelAvailability::Available,
    );
    model.options.push(ModelOptionDescriptor {
        id: ModelOptionId::new("fast"),
        label: "Fast".into(),
        description: None,
        role: ModelOptionRole::Speed,
        kind: ModelOptionKind::Toggle { default: false },
    });
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".into(),
                    models: vec![model],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .unwrap();
    let buffer = rendered_application_buffer(&application, 100, 32);
    let first = text_position(&buffer, "Sample");
    let last = text_position(&buffer, "Fast");
    let end = (last.0 + 3, last.1);
    assert_eq!(
        drag(&mut application, first, end),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_buffer(&application, 100, 32)[first]
            .modifier
            .contains(Modifier::REVERSED)
    );
    let copied = application
        .handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    assert_eq!(
        copied,
        ApplicationTransition::CopyToClipboard("Sample 界🙂 Model · Codex\n› Fast".into())
    );
    assert!(
        !rendered_application_buffer(&application, 100, 32)[first]
            .modifier
            .contains(Modifier::REVERSED)
    );
}
