//! Application shell: connection lifecycle views and responsive layout degradation.

use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{
        connected_application, connected_application_with_terminal_facts, enter_session,
        failed_session_snapshot, fixture_instance_id, model_descriptor, navigable_session_snapshot,
        ready_health, rendered_application_buffer, rendered_application_rows,
        rendered_application_rows_at, rendered_row, rendered_rows, text_position,
        type_terminal_text, workspace_dir,
    },
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use suru::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, SessionEvent,
    },
    protocol::{
        Activity, AgentSelection, CreateSessionRequest, InitialPrompt, ModelAvailability,
        ModelCatalog, ModelId, PromptId, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
        ServerShutdown, SessionId, SessionStatus, SessionTimestamp, ShutdownReason, TurnStatus,
        Workspace,
    },
    server::ServerConfig,
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, TerminalFacts,
        command_for_terminal_event,
    },
};
use uuid::Uuid;

fn rendered_state_rows(application: &Application) -> Vec<String> {
    rendered_rows(|frame| application.render(frame))
}

/// A client reads its launching Workspace the way the server reads one, and a
/// directory that cannot be read that way is not a reason to refuse to start:
/// the client stands on the path as given, and the Landing says so.
#[test]
fn a_launch_directory_that_cannot_be_canonicalized_still_starts_on_the_path_as_given() {
    let root = tempfile::tempdir().expect("create fixture root");
    let missing = root.path().join("missing");

    let application = connected_application(&missing);

    let landing = rendered_application_rows_at(&application, 100, 20).join("\n");
    assert!(
        landing.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"),
        "the client starts on the Workspace it cannot read: {landing:?}"
    );
    assert!(
        landing.contains(&format!("Workspace {}", missing.to_string_lossy())),
        "and the footer says where, by the spelling the client was given: {landing:?}"
    );
}

fn connected_state(instance_id: Uuid, pid: u32) -> Application {
    let mut application = Application::default();
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, pid),
        )))
        .expect("connect Application");
    application
}

#[test]
fn headless_application_handles_terminal_and_managed_events_through_the_production_renderer() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut application = Application::default();

    let managed_transition = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("handle connected event");
    assert_eq!(managed_transition, ApplicationTransition::Continue);

    let screen = rendered_application_rows(&application).join("\n");
    assert!(screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(screen.contains("Type a prompt"));
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));

    let insert = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))
    .expect("map terminal input to a semantic command");
    assert_eq!(insert, CommandId::InsertText("q".to_owned()));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(insert))
            .expect("insert printable input"),
        ApplicationTransition::Continue
    );
    let quit = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .expect("map Ctrl+C to a semantic command");
    assert_eq!(quit, CommandId::ClearOrExit);
    let clear_transition = application
        .handle_event(ApplicationEvent::Command(quit))
        .expect("handle terminal command");
    assert_eq!(clear_transition, ApplicationTransition::Continue);
    let terminal_transition = application
        .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
        .expect("handle terminal command after clearing the composer");
    assert_eq!(terminal_transition, ApplicationTransition::Exit);
}

#[test]
fn connected_application_uses_the_persisted_landing_agent_selection() {
    let workspace = workspace_dir();
    let selected = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-remembered"),
        options: Vec::new(),
    };
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424)
                .with_landing_agent_selection(Some(selected.clone())),
        )))
        .expect("connect with persisted landing Agent Selection");

    let before_catalog = rendered_application_rows_at(&application, 100, 16).join("\n");
    assert!(before_catalog.contains("codex · gpt-remembered"));
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open the Model picker")
    else {
        panic!("the Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "gpt-remembered",
                        "Remembered GPT",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("load matching Model catalog");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close the Model picker without changing the selection");
    let after_catalog = rendered_application_rows_at(&application, 100, 16).join("\n");
    assert!(after_catalog.contains("codex · gpt-remembered"));
    assert!(!after_catalog.contains("Remembered GPT"));

    type_terminal_text(&mut application, "Use the remembered Agent");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit with persisted landing Agent Selection")
    else {
        panic!("landing Prompt should create a Session");
    };
    assert_eq!(request.agent_selection, Some(selected));
}

#[tokio::test]
async fn headless_application_creates_a_session_and_renders_its_first_turn_through_the_managed_client()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = workspace_dir();
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "headless-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "headless-session-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let mut application = Application::new(workspace.path(), Default::default());

    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(landing.contains("Type a prompt"));

    for character in "Explain this workspace".chars() {
        let command = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )))
        .expect("map typed character to a semantic command");
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(command))
                .expect("edit landing composer"),
            ApplicationTransition::Continue
        );
    }
    let submit = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("map Enter to submit command");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(submit))
        .expect("submit first Prompt")
    else {
        panic!("first Prompt should request atomic Session creation");
    };
    let created = client
        .create_session(request)
        .await
        .expect("create Session through managed client");
    application
        .handle_event(ApplicationEvent::SessionCreated(created.clone()))
        .expect("transition application after Session creation");
    let mut subscription = client
        .attach_session(created.session.id)
        .await
        .expect("subscribe to created Session");
    let session_event = subscription
        .next()
        .await
        .expect("Session snapshot arrives")
        .expect("Session snapshot is valid");
    let SessionEvent::Snapshot(authoritative) = &session_event else {
        panic!("Session attachment begins with an authoritative snapshot");
    };
    assert_eq!(authoritative.session.id, created.session.id);
    let authoritative = authoritative.clone();
    application
        .handle_event(ApplicationEvent::Session(session_event))
        .expect("hydrate the Session route from its stream");

    let transcript = rendered_application_rows(&application).join("\n");
    assert!(transcript.contains("Explain this workspace"));
    assert!(transcript.contains("No Provider runtime is configured"));
    assert!(
        transcript.contains(
            std::fs::canonicalize(workspace.path())
                .expect("canonicalize expected Workspace")
                .to_string_lossy()
                .as_ref()
        )
    );

    let mut observer = Application::new(workspace.path(), Default::default());
    observer
        .handle_event(ApplicationEvent::SessionAttached(
            authoritative.as_ref().clone(),
        ))
        .expect("attach an independent observer to the Session");
    observer
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Observer-only draft".to_owned(),
        )))
        .expect("edit the observer's local draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep this unsent draft".to_owned(),
        )))
        .expect("edit the client-local Session draft");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            authoritative.as_ref().clone(),
        )))
        .expect("rehydrate the Session from a fresh snapshot");
    let with_draft = rendered_application_rows(&application).join("\n");
    let observer_with_draft = rendered_application_rows(&observer).join("\n");
    assert!(with_draft.contains("Keep this unsent draft"));
    assert!(!with_draft.contains("Observer-only draft"));
    assert!(observer_with_draft.contains("Observer-only draft"));
    assert!(!observer_with_draft.contains("Keep this unsent draft"));

    let replacement_instance = Uuid::new_v4();
    let original_instance = server.descriptor().instance_id;
    let mut attached_with_landing_draft = Application::new(workspace.path(), Default::default());
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(original_instance, server.descriptor().pid),
        )))
        .expect("connect attached client to original server");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Saved landing draft".to_owned(),
        )))
        .expect("edit separate landing draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::SessionAttached(created.clone()))
        .expect("attach while retaining the landing draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Recovered Session draft".to_owned(),
        )))
        .expect("edit attached Session draft");
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(replacement_instance, 84_848),
        )))
        .expect("connect attached client to replacement");
    let recovered_collision = rendered_application_rows(&attached_with_landing_draft).join("\n");
    assert!(recovered_collision.contains("Recovered Session draft"));
    let previous_draft = command_for_terminal_event(InputEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )))
    .expect("map Up to local draft history");
    assert_eq!(previous_draft, CommandId::HistoryPrevious);
    attached_with_landing_draft
        .handle_event(ApplicationEvent::Command(previous_draft))
        .expect("recall the displaced landing draft");
    assert!(
        rendered_application_rows(&attached_with_landing_draft)
            .join("\n")
            .contains("Saved landing draft")
    );

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: Duration::ZERO,
            },
        )))
        .expect("handle replacement recovery");
    let replacement_transition = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(replacement_instance, 84_848),
        )))
        .expect("handle replacement connection");
    assert_eq!(replacement_transition, ApplicationTransition::SessionEnded);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(created)))
        .expect("ignore a queued event from the ended Session");
    let after_replacement = rendered_application_rows(&application).join("\n");
    assert!(after_replacement.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(after_replacement.contains("Keep this unsent draft"));
    assert!(after_replacement.contains("Session ended because the shared server was replaced"));
    assert!(!after_replacement.contains("Explain this workspace"));
    assert!(!after_replacement.contains("No Agent is selected"));

    drop(subscription);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[test]
fn connecting_view_exposes_connection_state_before_a_snapshot_arrives() {
    let screen = rendered_state_rows(&Application::default()).join("\n");

    assert!(screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(screen.contains("Type a prompt"));
    assert!(screen.contains("Connecting to Suru server..."));
}

#[test]
fn connected_view_centers_the_landing_composer_and_shows_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let state = connected_state(instance_id, 42_424);

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(screen.contains("Type a prompt"));
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn landing_centers_the_logo_and_composer_together_as_the_draft_grows() {
    let workspace = workspace_dir();
    for draft in ["", "one\ntwo\nthree\nfour"] {
        let mut application = connected_application(workspace.path());
        type_terminal_text(&mut application, draft);
        for (width, height) in [(80, 20), (80, 21), (43, 20)] {
            let rows = rendered_application_rows_at(&application, width, height);
            let logo_top = rendered_row(&rows, "█          ▀▀▀▀▀▀▀█");
            let logo_bottom = rendered_row(&rows, "▀▄▄▀▄▄▀");
            let composer_top = rendered_row(&rows, "┌");
            let composer_bottom = rendered_row(&rows, "└");
            let footer = rendered_row(&rows, "Connected");
            let below = footer - composer_bottom - 1;
            assert!(
                logo_top.abs_diff(below) <= 1,
                "logo and composer share equal space above and below: {rows:?}"
            );
            assert_eq!(logo_bottom - logo_top, 6);
            assert_eq!(composer_top - logo_bottom, 2, "one blank row before input");
            let logo_left = rows[logo_top..=logo_bottom]
                .iter()
                .filter_map(|row| row.chars().position(|ch| ch != ' '))
                .min()
                .unwrap();
            let logo_right = rows[logo_top..=logo_bottom]
                .iter()
                .map(|row| row.trim_end().chars().count())
                .max()
                .unwrap();
            assert!(
                (logo_left + 1).abs_diff(width as usize - logo_right - 1) <= 1,
                "the logo sits one column left of center for visual balance"
            );
        }
    }
}

#[test]
fn landing_shell_degrades_by_priority_without_sacrificing_the_composer() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep the composer usable".to_owned(),
        )))
        .expect("type a landing draft");

    let wide = rendered_application_rows_at(&application, 80, 16).join("\n");
    for content in [
        "Keep the composer usable",
        "Agent unavailable",
        "Workspace",
        "Connected",
    ] {
        assert!(
            wide.contains(content),
            "wide landing frame omitted {content:?}"
        );
    }

    assert!(wide.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(!wide.contains("What would you like to work on?"));

    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    for core in ["Keep the composer usable", "Agent unavailable", "Connected"] {
        assert!(
            narrow.contains(core),
            "narrow landing frame omitted {core:?}"
        );
    }
    assert!(!narrow.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    for secondary in ["Workspace", "Provider", "Model", "Enter submit"] {
        assert!(
            !narrow.contains(secondary),
            "narrow landing frame retained secondary metadata {secondary:?}"
        );
    }

    let short = rendered_application_rows_at(&application, 80, 6).join("\n");
    assert!(!short.contains("Suru"));
    assert!(!short.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(short.contains("Keep the composer usable"));
    assert!(short.contains("Connected"));

    let too_small = rendered_application_rows_at(&application, 24, 4).join("\n");
    assert!(too_small.contains("Terminal too small"));
    assert!(!too_small.contains("Keep the composer usable"));
    assert!(!too_small.contains("Type a prompt"));
}

#[test]
fn working_indicator_is_the_transient_tail_of_the_transcript() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    snapshot.turns[0].status = TurnStatus::Active;
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Status {
        id: activity_id,
        turn_id: snapshot.turns[0].id,
        text: "Provider activity".to_owned(),
    };

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Working Session");
    let rows = rendered_application_rows_at(&application, 100, 16);
    let transcript_tail = rendered_row(&rows, "Provider activity");
    let indicator = rendered_row(&rows, "Working (0s • Esc to interrupt)");
    assert_eq!(
        indicator,
        transcript_tail + 2,
        "one empty row separates the Working Indicator from the latest Transcript row"
    );
    assert!(
        rows[indicator - 1].trim().is_empty(),
        "the Working Indicator has breathing room above it"
    );
    assert!(
        rows[indicator + 1].trim().is_empty(),
        "layout's Transcript margin follows the indicator"
    );
    assert!(
        !rows.join("\n").contains("active"),
        "the composer footer no longer carries an active status"
    );
}

#[test]
fn working_indicator_shimmers_only_its_state_label() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    snapshot.turns[0].status = TurnStatus::Active;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Working Session");

    let before = rendered_application_buffer(&application, 100, 16);
    let (label_x, label_y) = text_position(&before, "Working");
    let (metadata_x, metadata_y) = text_position(&before, "Esc to interrupt");
    let label_before = (0.."Working".len())
        .map(|offset| {
            before
                .cell((label_x + offset as u16, label_y))
                .unwrap()
                .style()
        })
        .collect::<Vec<_>>();
    let metadata_before = before.cell((metadata_x, metadata_y)).unwrap().style();

    for _ in 0..10 {
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .expect("advance presentation animation");
    }
    let after = rendered_application_buffer(&application, 100, 16);
    let label_after = (0.."Working".len())
        .map(|offset| {
            after
                .cell((label_x + offset as u16, label_y))
                .unwrap()
                .style()
        })
        .collect::<Vec<_>>();
    assert_ne!(
        label_after, label_before,
        "the state label advances its shimmer"
    );
    assert_eq!(
        after.cell((metadata_x, metadata_y)).unwrap().style(),
        metadata_before,
        "elapsed time and interrupt guidance remain visually stable"
    );
}

#[test]
fn a_terminal_without_truecolor_uses_the_modifier_shimmer() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    snapshot.turns[0].status = TurnStatus::Active;
    let mut application =
        connected_application_with_terminal_facts(workspace.path(), TerminalFacts::unprobed(false));
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Working Session");

    let opening = rendered_application_buffer(&application, 100, 16);
    let (label_x, label_y) = text_position(&opening, "Working");
    let opening_styles = (0.."Working".len())
        .map(|offset| {
            opening
                .cell((label_x + offset as u16, label_y))
                .unwrap()
                .style()
        })
        .collect::<Vec<_>>();
    assert!(opening_styles.iter().all(|style| {
        style.fg == Some(ratatui::style::Color::Reset)
            && style.add_modifier.contains(ratatui::style::Modifier::DIM)
    }));

    for _ in 0..10 {
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .expect("advance presentation animation");
    }
    let sweeping = rendered_application_buffer(&application, 100, 16);
    let sweeping_styles = (0.."Working".len())
        .map(|offset| {
            sweeping
                .cell((label_x + offset as u16, label_y))
                .unwrap()
                .style()
        })
        .collect::<Vec<_>>();
    assert!(
        sweeping_styles
            .iter()
            .all(|style| style.fg == Some(ratatui::style::Color::Reset))
    );
    assert!(
        sweeping_styles
            .iter()
            .any(|style| style.add_modifier.contains(ratatui::style::Modifier::BOLD))
    );
    assert!(
        sweeping_styles
            .iter()
            .any(|style| style.add_modifier.contains(ratatui::style::Modifier::DIM))
    );

    application.set_terminal_facts(TerminalFacts::unprobed(true));
    let truecolor = rendered_application_buffer(&application, 100, 16);
    assert!((0.."Working".len()).all(|offset| {
        matches!(
            truecolor
                .cell((label_x + offset as u16, label_y))
                .unwrap()
                .fg,
            ratatui::style::Color::Rgb(..)
        )
    }));
}

#[test]
fn working_indicator_end_truncates_without_wrapping_on_a_narrow_terminal() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Idle;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session waiting for Subagents");

    let rows = rendered_application_rows_at(&application, 28, 14);
    let indicator_rows = rows
        .iter()
        .filter(|row| row.contains("Waiting for subagents"))
        .collect::<Vec<_>>();
    assert_eq!(
        indicator_rows.len(),
        1,
        "the Working Indicator remains exactly one row: {rows:?}"
    );
    assert!(
        indicator_rows[0].contains('…'),
        "metadata end-truncates inside the content column: {rows:?}"
    );
    assert!(
        !rows.join("\n").contains("to interrupt"),
        "truncated metadata does not wrap onto a second row: {rows:?}"
    );
}

#[test]
fn working_indicator_scrolls_away_with_the_transcript_tail() {
    let workspace = workspace_dir();
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 20);
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    snapshot
        .turns
        .last_mut()
        .expect("fixture has a Turn")
        .status = TurnStatus::Active;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a long Working Session");

    let following = rendered_application_rows_at(&application, 60, 14).join("\n");
    assert!(following.contains("Working ("));
    assert!(!following.contains("Latest ↓"));

    application
        .handle_event(ApplicationEvent::Command(CommandId::ScrollTranscriptPageUp))
        .expect("move into Transcript history");
    let history = rendered_application_rows_at(&application, 60, 14).join("\n");
    assert!(history.contains("Latest ↓"));
    assert!(
        !history.contains("Working ("),
        "the transient indicator belongs to the Transcript tail: {history}"
    );
}

#[test]
fn session_shell_degrades_metadata_before_transcript_or_composer_content() {
    let workspace = workspace_dir();
    let mut active_snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    active_snapshot.session.status = SessionStatus::Active;
    active_snapshot.session.working_since = Some(SessionTimestamp::now());
    active_snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("openai"),
        model: ModelId::new("gpt-5"),
        options: Vec::new(),
    });
    active_snapshot.turns[0].status = TurnStatus::Active;
    let activity_id = active_snapshot.activities[0].id();
    active_snapshot.activities[0] = Activity::Status {
        id: activity_id,
        turn_id: active_snapshot.turns[0].id,
        text: "Working".to_owned(),
    };

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(active_snapshot.clone()))
        .expect("attach active Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep the draft visible".to_owned(),
        )))
        .expect("type a Session draft");

    let wide = rendered_application_rows_at(&application, 100, 16).join("\n");
    for content in [
        "Suru",
        "Workspace",
        workspace.path().to_string_lossy().as_ref(),
        "Connected",
        "Keep the transcript visible",
        "Keep the draft visible",
        "Working (",
        "Esc to interrupt",
        "openai · gpt-5",
    ] {
        assert!(
            wide.contains(content),
            "wide Session frame omitted {content:?}"
        );
    }

    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    for core in [
        "Suru",
        "Connected",
        "Keep the draft visible",
        "Working (",
        "Esc to interrupt",
    ] {
        assert!(
            narrow.contains(core),
            "narrow Session frame omitted {core:?}"
        );
    }
    for secondary in ["Workspace", "openai", "Enter submit"] {
        assert!(
            !narrow.contains(secondary),
            "narrow Session frame retained secondary metadata {secondary:?}"
        );
    }

    let short = rendered_application_rows_at(&application, 100, 6).join("\n");
    assert!(!short.contains("Suru"));
    assert!(!short.contains("Workspace"));
    assert!(short.contains("Working"));
    assert!(short.contains("Keep the draft visible"));
    assert!(short.contains("Working ("));
    assert!(
        !short.contains("Connected"),
        "connection status belongs to the hidden Session header, not the composer footer: {short}"
    );

    let mut idle = Application::new(workspace.path(), Default::default());
    let mut idle_snapshot = active_snapshot;
    idle_snapshot.session.status = SessionStatus::Idle;
    idle_snapshot.session.working_since = None;
    idle_snapshot.session.agent_selection = None;
    idle.handle_event(ApplicationEvent::SessionAttached(idle_snapshot))
        .expect("attach unavailable-Agent Session");
    let idle_frame = rendered_application_rows_at(&idle, 80, 12).join("\n");
    assert!(idle_frame.contains("Agent unavailable"));
    assert!(!idle_frame.contains("Working ("));
    assert!(!idle_frame.contains("Esc to interrupt"));
    assert!(!idle_frame.contains("Provider"));
    assert!(!idle_frame.contains("Model"));

    let too_small = rendered_application_rows_at(&application, 24, 4).join("\n");
    assert!(too_small.contains("Terminal too small"));
    assert!(!too_small.contains("Keep the transcript visible"));
    assert!(!too_small.contains("Keep the draft visible"));
}

#[test]
fn recovering_view_retains_the_landing_composer_and_last_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 2,
                retry_in: Duration::from_millis(500),
            },
        )))
        .expect("begin recovery");

    let screen = rendered_state_rows(&state).join("\n");

    assert!(screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(screen.contains("Recovering"));
    assert!(screen.contains("pid 42424"));
}

#[test]
fn reconnect_overlay_waits_for_the_grace_period_and_blocks_composer_input() {
    let workspace = workspace_dir();
    let instance_id = fixture_instance_id();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Keep the Session visible",
            workspace.path(),
        )))
        .expect("attach Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Preserve this draft".to_owned(),
        )))
        .expect("type Session draft");

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: Duration::from_millis(50),
            },
        )))
        .expect("begin recovery");
    let brief = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(brief.contains("Recovering"));
    assert!(brief.contains("Keep the Session visible"));
    assert!(brief.contains("Preserve this draft"));
    assert!(!brief.contains("Reconnecting to Suru"));
    assert!(!brief.contains("Your Session will resume automatically"));

    application
        .handle_event(ApplicationEvent::ReconnectGraceElapsed)
        .expect("show delayed reconnect overlay");
    let prolonged = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(prolonged.contains("Reconnecting to Suru"));
    assert!(prolonged.contains("Your Session will resume automatically"));
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            " must stay blocked".to_owned(),
        )))
        .expect("reconnect mode owns composer input");

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("reconnect to surviving server");
    let recovered = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(!recovered.contains("Reconnecting to Suru"));
    assert!(recovered.contains("Keep the Session visible"));
    assert!(recovered.contains("Preserve this draft"));
    assert!(!recovered.contains("must stay blocked"));
    assert!(recovered.contains("Connected"));
}

#[test]
fn recovered_view_switches_identity_on_the_confirmed_connection() {
    let previous_instance_id = Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002")
        .expect("parse previous instance ID");
    let recovered_instance_id = Uuid::parse_str("a4cc72ad-5507-4d4f-89f4-a3f7f1119d41")
        .expect("parse recovered instance ID");
    let mut state = connected_state(previous_instance_id, 42_424);
    state
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: Duration::ZERO,
            },
        )))
        .expect("begin recovery");
    state
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(recovered_instance_id, 84_848),
        )))
        .expect("finish recovery");

    let recovered = rendered_state_rows(&state).join("\n");
    assert!(recovered.contains("Connected"));
    assert!(recovered.contains("pid 84848"));
    assert!(recovered.contains("a4cc72ad"));
}

#[test]
fn manual_stop_view_retains_the_landing_screen_and_last_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state
        .handle_event(ApplicationEvent::Managed(ManagedEvent::ServerShutdown(
            ServerShutdown {
                instance_id,
                reason: ShutdownReason::Manual,
            },
        )))
        .expect("stop server");

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(screen.contains("Shared server stopped intentionally"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn ended_session_subscription_requests_a_fresh_snapshot_for_reconciliation() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, _) = enter_session(&mut application, workspace.path());

    assert_eq!(
        application
            .handle_event(ApplicationEvent::SessionSubscriptionEnded)
            .expect("handle ended Session subscription"),
        ApplicationTransition::SubscribeSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            session_id,
        ))
    );
}

/// `/settle` end to end: the slash command a reader types in an open Session
/// reaches a real server, and the Session it names comes back set aside.
///
/// The command is followed from the keystroke to what the server holds, which
/// is the only place the two halves meet — the transition names the Session,
/// and the server is what decides it is settled.
#[tokio::test]
async fn headless_slash_settle_sets_the_open_session_aside_on_a_real_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = workspace_dir();
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "headless-settle-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "headless-settle-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let mut application = Application::new(workspace.path(), Default::default());

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session through managed client");
    let session_id = created.session.id;
    application
        .handle_event(ApplicationEvent::SessionCreated(created))
        .expect("open the created Session");

    type_terminal_text(&mut application, "/settle");
    let ApplicationTransition::SettleSession {
        session: named,
        settled: set_aside,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("invoke the settle command")
    else {
        panic!("/settle in an open Session should ask for that Session to be set aside");
    };
    assert_eq!(
        named.session_id, session_id,
        "the command names the Session the reader is in"
    );
    assert_eq!(named.origin, suru::protocol::Outlook::Local);
    assert!(
        set_aside,
        "and asks for it to be set aside rather than brought back"
    );

    let settled = client
        .settle_session(named.session_id, true)
        .await
        .expect("the server accepts the Session the command named");
    assert!(
        settled.settled_at.is_some(),
        "the Session the command named comes back set aside"
    );
    assert_eq!(
        client
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .into_iter()
            .find(|item| item.id() == session_id)
            .expect("the Session remains listed")
            .settled_at(),
        settled.settled_at,
        "every listing of that Session now carries the marker"
    );

    server.shutdown().await.expect("stop server");
}
