//! The session picker: ordering, search, scope, attachment, and deletion.

use crate::support::{
    connected_application, enter_active_session, failed_session_snapshot,
    navigable_session_snapshot, rendered_application_rows, rendered_application_rows_at,
    rendered_row, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        ModelAvailability, PromptId, Session, SessionCatalogRevision, SessionCatalogSnapshot,
        SessionDeleted, SessionId, SessionListItem, SessionStatus, SessionSummary,
        SessionTimestamp, UnreadableSessionSummary, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SessionListRequest,
        SessionListScope,
    },
};

#[test]
fn sessions_command_opens_a_loading_picker_for_the_current_workspace() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    type_terminal_text(&mut application, "/sessions");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/sessions")
    );
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /sessions"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Sessions"));
    assert!(picker.contains("Loading"));
}

#[test]
fn session_picker_orders_marks_focuses_and_wraps_live_sessions() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (current_id, _, _) = enter_active_session(&mut application, workspace.path());
    let newest_id = SessionId::new();
    let oldest_id = SessionId::new();

    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                session_summary(
                    oldest_id,
                    workspace.path(),
                    "Oldest work",
                    SessionStatus::Idle,
                    10,
                ),
                session_summary(
                    newest_id,
                    workspace.path(),
                    "Newest work",
                    SessionStatus::Idle,
                    30,
                ),
                session_summary(
                    current_id,
                    workspace.path(),
                    "Current work",
                    SessionStatus::Active,
                    20,
                ),
            ],
        })
        .expect("hydrate Session picker");

    let rows = rendered_application_rows(&application);
    assert!(rendered_row(&rows, "Newest work") < rendered_row(&rows, "Current work"));
    assert!(rendered_row(&rows, "Current work") < rendered_row(&rows, "Oldest work"));
    let current_row = rows
        .iter()
        .find(|row| row.contains("Current work"))
        .expect("render current Session row");
    assert!(current_row.contains("current"));
    assert!(current_row.contains("active"));
    assert!(current_row.contains('›'));
    for title in ["Newest work", "Current work", "Oldest work"] {
        assert!(
            !rows
                .iter()
                .find(|row| row.contains(title))
                .expect("render Session picker row")
                .contains(workspace.path().to_string_lossy().as_ref())
        );
    }
    assert!(rows.join("\n").contains("ago"));

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .expect("wrap Session picker selection");
    }
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select wrapped Session row"),
        ApplicationTransition::AttachSession(newest_id)
    );
}

#[test]
fn session_picker_requires_confirmation_and_removes_authoritatively_deleted_session() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    let selected_id = SessionId::new();
    let remaining_id = SessionId::new();
    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                selected_id,
                workspace.path(),
                "Delete this Session",
                SessionStatus::Idle,
                20,
            ),
            session_summary(
                remaining_id,
                workspace.path(),
                "Keep this Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("request Session deletion confirmation"),
        ApplicationTransition::Continue
    );
    let confirming = rendered_application_rows(&application).join("\n");
    assert!(confirming.contains("Press Ctrl+D again to confirm"));
    assert!(confirming.contains("Keep this Session"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .expect("confirm Session deletion"),
        ApplicationTransition::DeleteSession(selected_id)
    );
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted {
                session_id: selected_id,
            },
        )))
        .expect("apply authoritative Session deletion");

    let deleted = rendered_application_rows(&application).join("\n");
    assert!(!deleted.contains("Delete this Session"));
    assert!(deleted.contains("Keep this Session"));
}

#[test]
fn reconnect_catalog_removes_a_missed_current_session_deletion() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    let (current_id, _, _) = enter_active_session(&mut application, workspace.path());
    let remaining_id = SessionId::new();
    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                current_id,
                workspace.path(),
                "Current Session",
                SessionStatus::Active,
                20,
            ),
            session_summary(
                remaining_id,
                workspace.path(),
                "Remaining Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Managed(
                ManagedEvent::SessionCatalogReconciled(SessionCatalogSnapshot {
                    revision: SessionCatalogRevision::INITIAL,
                    session_ids: vec![remaining_id],
                }),
            ))
            .expect("reconcile deletion missed during a catalog disconnect"),
        ApplicationTransition::SessionEnded
    );
    let picker = rendered_application_rows(&application).join("\n");
    assert!(!picker.contains("Current Session"));
    assert!(picker.contains("Remaining Session"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close Session picker");
    let landing = rendered_application_rows(&application).join("\n");
    assert!(!landing.contains("Long-running work"));
    assert!(landing.contains("Session ended because it was deleted"));
}

#[test]
fn session_attachment_failure_preserves_the_original_until_target_hydration() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (original_id, original_snapshot, _) =
        enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "preserved draft".to_owned(),
        )))
        .expect("type original Session draft");
    let target_id = SessionId::new();

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                20,
            ),
            session_summary(
                target_id,
                workspace.path(),
                "Target Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus target Session");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("begin target attachment"),
        ApplicationTransition::AttachSession(target_id)
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Attaching")
    );

    let refresh_request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::SessionAttachmentFailed(
                "target disappeared".to_owned(),
            ))
            .expect("report failed attachment"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: refresh_request,
            sessions: vec![session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                30,
            )],
        })
        .expect("refresh point-in-time Session status");
    let failed = rendered_application_rows(&application).join("\n");
    assert!(failed.contains("target disappeared"));
    let tiny_failure = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(tiny_failure.contains("Error"));
    assert!(tiny_failure.contains("Original"));
    let short_failure = rendered_application_rows_at(&application, 28, 7).join("\n");
    assert!(short_failure.contains("Error"));
    assert!(short_failure.contains("Original"));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close picker after failed attachment");
    let original = rendered_application_rows(&application).join("\n");
    assert!(original.contains("Long-running work"));
    assert!(original.contains("preserved draft"));

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                original_id,
                workspace.path(),
                "Original Session",
                SessionStatus::Active,
                30,
            ),
            session_summary(
                target_id,
                workspace.path(),
                "Target Session",
                SessionStatus::Idle,
                20,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus target Session again");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("retry target attachment");
    let target_snapshot = failed_session_snapshot(
        target_id,
        PromptId::new(),
        "Target Session transcript",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(target_snapshot))
        .expect("hydrate target Session");
    let target = rendered_application_rows(&application).join("\n");
    assert!(target.contains("Target Session transcript"));
    assert!(!target.contains("Sessions"));

    application
        .handle_event(ApplicationEvent::SessionAttached(original_snapshot))
        .expect("return to original Session");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("preserved draft")
    );
}

#[test]
fn session_picker_searches_titles_and_remembers_all_workspace_scope() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("open semantic leader");
    let current_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::NONE,
            )))
            .expect("invoke Ctrl+X L"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let gamma_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request.clone(),
            sessions: vec![
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Alpha notes",
                    SessionStatus::Idle,
                    20,
                ),
                session_summary(
                    gamma_id,
                    workspace.path(),
                    "Gamma migration",
                    SessionStatus::Idle,
                    10,
                ),
            ],
        })
        .expect("hydrate current-Workspace Sessions");
    type_terminal_text(&mut application, "gMg");
    let filtered = rendered_application_rows(&application).join("\n");
    assert!(filtered.contains("Gamma migration"));
    assert!(!filtered.contains("Alpha notes"));

    let all_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("toggle all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    let loading = rendered_application_rows(&application).join("\n");
    assert!(loading.contains("All Workspaces"));
    assert!(loading.contains("Loading"));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request,
            sessions: vec![session_summary(
                SessionId::new(),
                workspace.path(),
                "Stale current-Workspace result",
                SessionStatus::Idle,
                40,
            )],
        })
        .expect("ignore stale current-Workspace result");
    let still_loading = rendered_application_rows(&application).join("\n");
    assert!(still_loading.contains("Loading"));
    assert!(!still_loading.contains("Stale current-Workspace result"));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: all_request,
            sessions: vec![session_summary(
                gamma_id,
                std::path::Path::new("/ws-two"),
                "Gamma migration",
                SessionStatus::Idle,
                30,
            )],
        })
        .expect("hydrate all-Workspace Sessions");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/ws-two")
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close Session picker");
    type_terminal_text(&mut application, "/resume");
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke /resume alias"),
        SessionListScope::AllWorkspaces,
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close alias-opened picker");
    type_terminal_text(&mut application, "/continue");
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("invoke /continue alias"),
        SessionListScope::AllWorkspaces,
    );
}

#[test]
fn session_picker_switching_restores_each_transcript_viewport() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let first_id = SessionId::new();
    let first_snapshot = navigable_session_snapshot(first_id, workspace.path(), 8);
    let second_id = SessionId::new();
    let second_snapshot = navigable_session_snapshot(second_id, workspace.path(), 3);
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(first_snapshot.clone()))
        .expect("attach first Session");
    rendered_application_rows_at(&application, 72, 18);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into first Session history");
    let first_history = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(first_history.contains("Latest"));

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                first_id,
                workspace.path(),
                "First Session",
                SessionStatus::Idle,
                20,
            ),
            session_summary(
                second_id,
                workspace.path(),
                "Second Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus second Session");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("select second Session");
    application
        .handle_event(ApplicationEvent::SessionAttached(second_snapshot))
        .expect("hydrate second Session");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Agent section 3")
    );

    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                second_id,
                workspace.path(),
                "Second Session",
                SessionStatus::Idle,
                30,
            ),
            session_summary(
                first_id,
                workspace.path(),
                "First Session",
                SessionStatus::Idle,
                20,
            ),
        ],
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus first Session");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("select first Session");
    application
        .handle_event(ApplicationEvent::SessionAttached(first_snapshot))
        .expect("rehydrate first Session");
    let restored = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(restored.contains("Latest"));
    assert!(!restored.contains("Agent section 8"));
}

#[test]
fn session_picker_stays_searchable_at_supported_small_terminal_sizes() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let loading = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(loading.contains("Sessions"));
    assert!(loading.contains("Loading"));

    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Alpha",
                    SessionStatus::Idle,
                    10,
                ),
                session_summary(
                    SessionId::new(),
                    workspace.path(),
                    "Beta",
                    SessionStatus::Active,
                    20,
                ),
            ],
        })
        .expect("hydrate Session picker");
    assert!(
        rendered_application_rows_at(&application, 28, 5)
            .join("\n")
            .contains("Beta")
    );
    type_terminal_text(&mut application, "alp");
    assert!(
        rendered_application_rows_at(&application, 28, 5)
            .join("\n")
            .contains("Alpha")
    );
    let narrow = rendered_application_rows_at(&application, 43, 10).join("\n");
    assert!(narrow.contains("Search: alp"));
    assert!(narrow.contains("Ctrl+A"));
}

#[test]
fn session_picker_scroll_window_keeps_the_current_session_visible() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let current_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            current_id,
            PromptId::new(),
            "Current transcript",
            workspace.path(),
        )))
        .expect("attach current Session");
    let newest_id = SessionId::new();
    let mut sessions = (1..=12)
        .map(|index| {
            session_summary(
                if index == 12 {
                    newest_id
                } else {
                    SessionId::new()
                },
                workspace.path(),
                &format!("Recent Session {index}"),
                SessionStatus::Idle,
                100 + index,
            )
        })
        .collect::<Vec<_>>();
    sessions.push(session_summary(
        current_id,
        workspace.path(),
        "Current Session",
        SessionStatus::Idle,
        1,
    ));
    open_session_picker_with(&mut application, sessions);

    let picker = rendered_application_rows_at(&application, 80, 15).join("\n");
    assert!(picker.contains("Current Session"));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("wrap from current Session to newest Session");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select wrapped newest Session"),
        ApplicationTransition::AttachSession(newest_id)
    );
}

#[test]
fn unreadable_session_picker_rows_remain_navigable_without_attachment() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut sessions = vec![session_summary(
        SessionId::new(),
        workspace.path(),
        "Readable Session",
        SessionStatus::Idle,
        100,
    )];
    sessions.extend((1..=12).map(|index| {
        SessionListItem::Unreadable(UnreadableSessionSummary {
            id: SessionId::new(),
            title: format!("Unreadable Session {index}"),
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(100 - index),
            workspace: Some(Workspace {
                path: workspace.path().to_owned(),
            }),
        })
    }));
    open_session_picker_with(&mut application, sessions);

    for _ in 0..12 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .expect("navigate through unreadable Sessions");
    }
    let picker = rendered_application_rows_at(&application, 80, 10).join("\n");
    let oldest = picker
        .lines()
        .find(|row| row.contains("Unreadable Session 12"))
        .expect("selected unreadable Session is scrolled into view");
    assert!(oldest.contains("[unreadable]"));
    assert!(oldest.contains('›'));
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("unreadable Session has no attachment action"),
        ApplicationTransition::Continue
    );
}

#[test]
fn session_picker_reserves_required_metadata_before_truncating_long_titles() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let current_id = SessionId::new();
    let mut application = Application::new(workspace.path());
    let mut snapshot = failed_session_snapshot(
        current_id,
        PromptId::new(),
        "Current transcript",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach current active Session");
    let current_request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
    );
    let all_request = expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("toggle all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: current_request,
            sessions: vec![session_summary(
                SessionId::new(),
                workspace.path(),
                "stale",
                SessionStatus::Idle,
                20,
            )],
        })
        .expect("ignore previous scope result");
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: all_request,
            sessions: vec![session_summary(
                current_id,
                std::path::Path::new("/a/very/long/workspace/path/remote-two"),
                "A deliberately enormous Session title that must yield to metadata",
                SessionStatus::Active,
                10,
            )],
        })
        .expect("hydrate all-Workspace Session");

    let rows = rendered_application_rows_at(&application, 80, 15);
    let row = rows
        .iter()
        .find(|row| row.contains("current"))
        .expect("render current Session metadata");
    assert!(row.contains("active"));
    assert!(row.contains("ago"));
    assert!(row.contains("remote-two"));

    let intermediate_rows = rendered_application_rows_at(&application, 50, 15);
    let intermediate_row = intermediate_rows
        .iter()
        .find(|row| row.contains("current"))
        .expect("render current Session metadata at intermediate width");
    assert!(intermediate_row.contains("active"));
    assert!(intermediate_row.contains("ago"));
    assert!(intermediate_row.contains("-two"));
}

#[test]
fn session_picker_consumes_input_before_hidden_composer_actions() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let (session_id, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "do not submit this draft".to_owned(),
        )))
        .expect("type Session draft");
    open_session_picker_with(
        &mut application,
        vec![session_summary(
            session_id,
            workspace.path(),
            "Current Session",
            SessionStatus::Active,
            10,
        )],
    );

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::ALT,
            )))
            .expect("picker consumes hidden queue submission binding"),
        ApplicationTransition::Continue
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close Session picker");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("do not submit this draft")
    );
}

fn session_summary(
    session_id: SessionId,
    workspace: &std::path::Path,
    title: &str,
    status: SessionStatus,
    updated_at: u64,
) -> SessionListItem {
    SessionListItem::Readable(SessionSummary {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status,
        },
        title: title.to_owned(),
        emoji: None,
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(updated_at),
    })
}

fn open_session_picker_with(application: &mut Application, sessions: Vec<SessionListItem>) {
    let workspace = sessions
        .first()
        .and_then(SessionListItem::workspace)
        .map(|workspace| workspace.path.clone())
        .unwrap_or_else(|| std::env::current_dir().expect("read current Workspace"));
    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace(workspace),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate Session picker");
}

fn expect_session_list_request(
    transition: ApplicationTransition,
    expected_scope: SessionListScope,
) -> SessionListRequest {
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("expected Session listing transition");
    };
    assert_eq!(request.scope(), &expected_scope);
    request
}
