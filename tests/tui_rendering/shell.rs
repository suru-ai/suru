//! Application shell: connection lifecycle views and responsive layout degradation.

use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{
        connected_application, enter_session, failed_session_snapshot, fixture_instance_id,
        ready_health, rendered_application_rows, rendered_application_rows_at, rendered_rows,
        type_terminal_text,
    },
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use suru::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, SessionEvent,
    },
    protocol::{
        Activity, AgentSelection, ModelId, PromptId, ProviderId, ServerShutdown, SessionId,
        SessionStatus, ShutdownReason, TurnStatus,
    },
    server::ServerConfig,
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, TuiState,
        command_for_terminal_event, render,
    },
};
use uuid::Uuid;

fn rendered_state_rows(state: &TuiState) -> Vec<String> {
    rendered_rows(|frame| render(frame, state))
}

fn connected_state(instance_id: Uuid, pid: u32) -> TuiState {
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(ready_health(instance_id, pid)));
    state
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
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Prompt"));
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
    let workspace = tempfile::tempdir().expect("create Workspace");
    let selected = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-remembered"),
        options: Vec::new(),
    };
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424)
                .with_landing_agent_selection(Some(selected.clone())),
        )))
        .expect("connect with persisted landing Agent Selection");

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
    let workspace = tempfile::tempdir().expect("create valid Workspace");
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
    let mut application = Application::new(workspace.path());

    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("What would you like to work on?"));
    assert!(landing.contains("Prompt"));

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

    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(authoritative.clone()))
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
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            authoritative.clone(),
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
    let mut attached_with_landing_draft = Application::new(workspace.path());
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
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(created)))
        .expect("ignore a queued event from the ended Session");
    let after_replacement = rendered_application_rows(&application).join("\n");
    assert!(after_replacement.contains("What would you like to work on?"));
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
    let screen = rendered_state_rows(&TuiState::default()).join("\n");

    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Type a Prompt and press Enter"));
    assert!(screen.contains("Connecting to Suru server..."));
}

#[test]
fn connected_view_centers_the_landing_composer_and_shows_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let state = connected_state(instance_id, 42_424);

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Prompt"));
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn landing_shell_degrades_by_priority_without_sacrificing_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep the composer usable".to_owned(),
        )))
        .expect("type a landing draft");

    let wide = rendered_application_rows_at(&application, 80, 16).join("\n");
    for content in [
        "Suru",
        "What would you like to work on?",
        "Keep the composer usable",
        "Agent unavailable",
        "Workspace",
        "Connected",
        "Enter submit",
    ] {
        assert!(
            wide.contains(content),
            "wide landing frame omitted {content:?}"
        );
    }

    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    for core in [
        "Suru",
        "What would you like to work on?",
        "Keep the composer usable",
        "Agent unavailable",
        "Connected",
    ] {
        assert!(
            narrow.contains(core),
            "narrow landing frame omitted {core:?}"
        );
    }
    for secondary in ["Workspace", "Provider", "Model", "Enter submit"] {
        assert!(
            !narrow.contains(secondary),
            "narrow landing frame retained secondary metadata {secondary:?}"
        );
    }

    let short = rendered_application_rows_at(&application, 80, 6).join("\n");
    assert!(!short.contains("Suru"));
    assert!(short.contains("What would you like to work on?"));
    assert!(short.contains("Keep the composer usable"));
    assert!(short.contains("Connected"));

    let too_small = rendered_application_rows_at(&application, 24, 4).join("\n");
    assert!(too_small.contains("Terminal too small"));
    assert!(!too_small.contains("Keep the composer usable"));
    assert!(!too_small.contains("Prompt"));
}

#[test]
fn session_shell_degrades_metadata_before_transcript_or_composer_content() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut active_snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Keep the transcript visible",
        workspace.path(),
    );
    active_snapshot.session.status = SessionStatus::Active;
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
        "active",
        "Esc interrupt",
        "Provider openai",
        "Model gpt-5",
        "Enter submit",
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
        "Keep the transcript visible",
        "Keep the draft visible",
        "active",
        "Esc interrupt",
    ] {
        assert!(
            narrow.contains(core),
            "narrow Session frame omitted {core:?}"
        );
    }
    for secondary in ["Workspace", "Provider", "Model", "Enter submit"] {
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
    assert!(short.contains("active"));
    assert!(short.contains("Connected"));

    let mut idle = Application::new(workspace.path());
    let mut idle_snapshot = active_snapshot;
    idle_snapshot.session.status = SessionStatus::Idle;
    idle_snapshot.session.agent_selection = None;
    idle.handle_event(ApplicationEvent::SessionAttached(idle_snapshot))
        .expect("attach unavailable-Agent Session");
    let idle_frame = rendered_application_rows_at(&idle, 80, 12).join("\n");
    assert!(idle_frame.contains("idle"));
    assert!(idle_frame.contains("Agent unavailable"));
    assert!(!idle_frame.contains("Esc interrupt"));
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

    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 2,
        retry_in: Duration::from_millis(500),
    }));

    let screen = rendered_state_rows(&state).join("\n");

    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Recovering"));
    assert!(screen.contains("pid 42424"));
}

#[test]
fn reconnect_overlay_waits_for_the_grace_period_and_blocks_composer_input() {
    let workspace = tempfile::tempdir().expect("create Workspace");
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
    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 1,
        retry_in: Duration::ZERO,
    }));
    state.apply(ManagedEvent::Connected(ready_health(
        recovered_instance_id,
        84_848,
    )));

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

    state.apply(ManagedEvent::ServerShutdown(ServerShutdown {
        instance_id,
        reason: ShutdownReason::Manual,
    }));

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Shared server stopped intentionally"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn fatal_protocol_error_is_rendered_visibly_with_the_last_known_state() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = connected_state(instance_id, 42_424);

    state.apply(ManagedEvent::Fatal(
        "server sent unknown event type 'future_event'".to_owned(),
    ));

    let screen = rendered_state_rows(&state).join("\n");
    assert!(screen.contains("What would you like to work on?"));
    assert!(screen.contains("Connection failed"));
    assert!(screen.contains("unknown event type 'future_event'"));
}

#[test]
fn ended_session_subscription_requests_a_fresh_snapshot_for_reconciliation() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, _) = enter_session(&mut application, workspace.path());

    assert_eq!(
        application
            .handle_event(ApplicationEvent::SessionSubscriptionEnded)
            .expect("handle ended Session subscription"),
        ApplicationTransition::SubscribeSession(session_id)
    );
}
