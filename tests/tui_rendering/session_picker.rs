//! The session picker: ordering, search, scope, attachment, and deletion.

use crate::support::{
    connected_application, connected_application_homed, enter_active_session,
    failed_session_snapshot, named_workspace_path, navigable_session_snapshot,
    noncanonical_spelling, rendered_application_buffer, rendered_application_rows,
    rendered_application_rows_at, rendered_row, text_position, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, EmojiVisibility, ModelAvailability, Outlook, PromptId, Remote,
        RemoteStatus, Session, SessionCatalogRevision, SessionCatalogSnapshot, SessionCreated,
        SessionDeleted, SessionId, SessionListItem, SessionSettings, SessionStatus, SessionSummary,
        SessionTimestamp, SessionTitleChanged, SettingsSnapshot, SidebarSettings,
        SidebarVisibility, TitleSettings, UnreadableSessionSummary, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SessionListRequest,
        SessionListScope, SessionListSurface,
    },
};

#[test]
fn sessions_command_opens_a_loading_picker_for_the_current_workspace() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
    );

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Sessions"));
    assert!(picker.contains("Loading"));
}

/// The server canonicalizes the Workspace it narrows a listing by, so a
/// client asking for "where I am" has to ask in the same reading — a launch
/// spelling that differs from the canonical one would name a Workspace none
/// of its own Sessions match.
#[test]
fn the_current_workspace_scope_asks_in_the_servers_reading_of_the_launch_directory() {
    let workspace = workspace_dir();
    let mut application = Application::new(noncanonical_spelling(&workspace), Default::default());
    type_terminal_text(&mut application, "/sessions");

    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /sessions"),
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
    );
}

#[test]
fn session_picker_orders_marks_focuses_and_wraps_live_sessions() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (current_id, _, _) = enter_active_session(&mut application, workspace.path());
    let newest_id = SessionId::new();
    let oldest_id = SessionId::new();

    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
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
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            newest_id,
        ))
    );
}

#[test]
fn session_picker_requires_confirmation_and_removes_authoritatively_deleted_session() {
    let workspace = workspace_dir();
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
        ApplicationTransition::DeleteSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            selected_id,
        ))
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
    let workspace = workspace_dir();
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
                    workspace_paths: Default::default(),
                    revision: SessionCatalogRevision::INITIAL,
                    session_ids: vec![remaining_id],
                    checkout_states: Vec::new(),
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
fn session_picker_selection_opens_the_target_optimistically_and_keeps_the_old_draft() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            target_id,
        ))
    );
    let opening = rendered_application_rows(&application).join("\n");
    assert!(!opening.contains("Sessions"));
    assert!(!opening.contains("Long-running work"));

    application
        .handle_event(ApplicationEvent::SessionAttachmentFailed(
            "target disappeared".to_owned(),
        ))
        .expect("refresh point-in-time Session status");
    let failed = rendered_application_rows(&application).join("\n");
    assert!(failed.contains("target disappeared"));
    assert!(!failed.contains("Long-running work"));
    let tiny_failure = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(tiny_failure.contains("Error"));
    let short_failure = rendered_application_rows_at(&application, 28, 7).join("\n");
    assert!(short_failure.contains("Error"));

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
    let workspace = workspace_dir();
    // A second Workspace this test only names, rooted the way the running
    // platform roots one so the picker draws it back as spelled.
    let other_workspace = named_workspace_path("ws-two");
    let mut application = Application::new(workspace.path(), Default::default());
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
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
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
                &other_workspace,
                "Gamma migration",
                SessionStatus::Idle,
                30,
            )],
        })
        .expect("hydrate all-Workspace Sessions");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(other_workspace.to_string_lossy().as_ref())
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
fn session_picker_scope_cycles_through_current_all_and_everywhere() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());

    let current = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker");
    expect_session_list_request(
        current,
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Current Workspace")
    );

    let all = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("widen to all Workspaces");
    expect_session_list_request(all, SessionListScope::AllWorkspaces);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("All Workspaces")
    );

    let ApplicationTransition::ListEverywhereRemotes(request) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("widen to Everywhere")
    else {
        panic!("Everywhere first discovers the paired Remotes");
    };
    assert_eq!(request.surface(), SessionListSurface::SessionPicker);
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Everywhere")
    );

    let ApplicationTransition::ReconcileCatalogOrigins {
        catalog_origins,
        requests,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("narrow to the current Workspace")
    else {
        panic!("leaving Everywhere reconciles its Origin interests");
    };
    assert!(catalog_origins.is_empty());
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].outlook(), &Outlook::Local);
    assert_eq!(
        requests[0].scope(),
        &SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into())
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Current Workspace")
    );
}

#[test]
fn everywhere_picker_asks_each_origin_and_draws_one_tagged_recency_order() {
    let workspace = workspace_dir();
    // The picker's popup is a fixed width however wide the terminal is drawn,
    // so a Workspace path long enough spends the columns these Titles are
    // asserted on and truncates them away. Two things keep the paths short on
    // every platform: the local Server reports the fixture root as its home, so
    // its own Session labels `~`; and each foreign Session is given a Workspace
    // on its own machine below, rather than borrowing the local tempdir.
    let mut application = connected_application_homed(workspace.path());
    let current = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker");
    expect_session_list_request(
        current,
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
    );
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("widen to all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    let ApplicationTransition::ListEverywhereRemotes(discovery) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("widen to Everywhere")
    else {
        panic!("Everywhere first discovers its Origins");
    };

    let ApplicationTransition::ReconcileCatalogOrigins {
        catalog_origins,
        requests,
    } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![
                remote("studio", RemoteStatus::Available),
                remote("sleeping", RemoteStatus::Unavailable),
                remote("revoked", RemoteStatus::Revoked),
            ],
        })
        .expect("discover the picker's Origins")
    else {
        panic!("one Session listing should be requested per participating Origin");
    };
    assert_eq!(
        catalog_origins,
        std::collections::HashSet::from([
            Outlook::Remote("studio".to_owned()),
            Outlook::Remote("sleeping".to_owned()),
        ])
    );
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| {
        request.surface() == SessionListSurface::SessionPicker
            && request.scope() == &SessionListScope::AllWorkspaces
    }));

    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => vec![session_summary(
                SessionId::new(),
                workspace.path(),
                "Local middle",
                SessionStatus::Idle,
                20,
            )],
            Outlook::Remote(name) if name == "studio" => vec![session_summary(
                SessionId::new(),
                &named_workspace_path("studio"),
                "X [studio]",
                SessionStatus::Idle,
                30,
            )],
            Outlook::Remote(name) if name == "sleeping" => vec![session_summary(
                SessionId::new(),
                &named_workspace_path("sleeping"),
                "Sleeping oldest",
                SessionStatus::Idle,
                10,
            )],
            outlook => panic!("unexpected Origin: {outlook:?}"),
        };
        application
            .handle_event(ApplicationEvent::SessionsListed { request, sessions })
            .expect("merge one Origin's Session listing");
    }

    let rows = rendered_application_rows_at(&application, 120, 24);
    assert!(rendered_row(&rows, "X [studio]") < rendered_row(&rows, "Local middle"));
    assert!(rendered_row(&rows, "Local middle") < rendered_row(&rows, "Sleeping oldest"));
    let studio_row = rows
        .iter()
        .find(|row| row.contains("X [studio]"))
        .expect("draw the studio Session");
    assert_eq!(studio_row.matches("[studio]").count(), 2);
    assert!(
        studio_row.contains('›'),
        "the newest merged row is the initial navigation selection: {studio_row:?}"
    );
    assert!(
        rows.iter()
            .find(|row| row.contains("Sleeping oldest"))
            .expect("draw the sleeping Session")
            .contains("[sleeping]")
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.contains("Local middle"))
            .expect("draw the local Session")
            .contains("[local]")
    );
    let buffer = rendered_application_buffer(&application, 120, 24);
    let row = rendered_row(&rows, "X [studio]") as u16;
    let tag_columns = rows[usize::from(row)]
        .match_indices("[studio]")
        .map(|(column, _)| column as u16)
        .collect::<Vec<_>>();
    assert_ne!(
        buffer
            .cell((tag_columns[0], row))
            .expect("draw the Origin-like text in the title")
            .fg,
        Color::DarkGray,
        "title text that resembles an Origin tag keeps the row style"
    );
    assert_eq!(
        buffer
            .cell((tag_columns[1], row))
            .expect("draw the actual Origin tag")
            .fg,
        Color::DarkGray,
        "the foreign Origin tag is subdued independently of row focus"
    );
    assert_eq!(
        buffer
            .cell((tag_columns[1], row))
            .expect("draw the actual Origin tag")
            .bg,
        buffer
            .cell((tag_columns[0], row))
            .expect("draw the selected row title")
            .bg,
        "the subdued tag keeps the selected row's highlight"
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("move to the visually next merged row");
    let moved = rendered_application_rows_at(&application, 120, 24);
    assert!(
        moved
            .iter()
            .find(|row| row.contains("Local middle"))
            .expect("draw the local Session")
            .contains('›'),
        "navigation follows the same recency order the picker draws: {moved:?}"
    );

    let transition = application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::RemoteFailed {
                status: RemoteStatus::Revoked,
                message: "Pairing revoked".to_owned(),
            },
        })
        .expect("end the studio Origin");
    let ApplicationTransition::ReconcileCatalogOrigins {
        catalog_origins, ..
    } = transition
    else {
        panic!("ending a background Origin reconciles catalog ownership");
    };
    assert!(!catalog_origins.contains(&Outlook::Remote("studio".to_owned())));
    assert!(
        !rendered_application_rows_at(&application, 120, 24)
            .join("\n")
            .contains("X [studio]"),
        "an ended Origin takes its picker rows with it"
    );
}

#[test]
fn choosing_a_foreign_picker_row_turns_and_opens_without_moving_sidebar_scope() {
    let workspace = workspace_dir();
    // A Workspace on the studio machine, not a subdirectory of the local
    // tempdir. A foreign Origin labels its own paths, and this fixture hands
    // over no paths for `studio`, so its Workspace is drawn exactly as spelled
    // — and the picker's popup is a fixed width however wide the terminal is
    // drawn, so a forty-column Windows temp path here would spend the columns
    // `Foreign work` is asserted on and truncate the Title away.
    let foreign_workspace = named_workspace_path("studio-work");
    let target = SessionId::new();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    sidebar: SidebarSettings {
                        initial_visibility: SidebarVisibility::Shown,
                        ..SidebarSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("show the Sidebar on its default all-Workspaces scope");

    let current = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker");
    expect_session_list_request(
        current,
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
    );
    expect_session_list_request(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
            )))
            .expect("widen to all Workspaces"),
        SessionListScope::AllWorkspaces,
    );
    let ApplicationTransition::ListEverywhereRemotes(discovery) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("widen the picker to Everywhere")
    else {
        panic!("Everywhere first discovers its Origins");
    };
    let before = rendered_application_rows_at(&application, 120, 24);
    assert!(before.iter().any(|row| row.contains("▸ All Workspaces")));
    assert!(before.join("\n").contains("Everywhere"));

    let ApplicationTransition::ReconcileCatalogOrigins { requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![remote("studio", RemoteStatus::Available)],
        })
        .expect("discover the studio Origin")
    else {
        panic!("the discovered Origins should each be listed");
    };
    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => Vec::new(),
            Outlook::Remote(name) if name == "studio" => vec![session_summary(
                target,
                &foreign_workspace,
                "Foreign work",
                SessionStatus::Idle,
                20,
            )],
            outlook => panic!("unexpected Origin: {outlook:?}"),
        };
        application
            .handle_event(ApplicationEvent::SessionsListed { request, sessions })
            .expect("merge the Origin's Sessions");
    }

    let reference =
        suru::protocol::SessionReference::new(Outlook::Remote("studio".to_owned()), target);
    let picker = rendered_application_rows_at(&application, 120, 24);
    assert!(
        picker
            .iter()
            .find(|row| row.contains("Foreign work"))
            .expect("draw the foreign row")
            .contains('›'),
        "the only row is selected: {picker:?}"
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("choose the foreign Session"),
        ApplicationTransition::TurnOutlookAndViewAndAttach {
            catalog_origins: std::collections::HashSet::from([Outlook::Remote(
                "studio".to_owned(),
            )]),
            session: reference.clone(),
        }
    );
    let after = rendered_application_rows_at(&application, 120, 24);
    assert!(
        after.iter().any(|row| row.contains("▸ All Workspaces")),
        "the Sidebar keeps its own scope when the picker turns the Outlook: {after:?}"
    );
    let ApplicationTransition::ListEverywhereRemotes(reopened) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::SessionList,
        )))
        .expect("reopen the Session picker")
    else {
        panic!("the Session picker keeps its own Everywhere scope across the turn");
    };
    assert_eq!(reopened.surface(), SessionListSurface::SessionPicker);
}

#[test]
fn an_open_everywhere_picker_catches_up_only_the_origin_whose_catalog_moved() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("advance to all Workspaces");
    let ApplicationTransition::ListEverywhereRemotes(discovery) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("advance the Session picker to Everywhere")
    else {
        panic!("Everywhere first discovers its Origins");
    };
    let ApplicationTransition::ReconcileCatalogOrigins { requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![remote("studio", RemoteStatus::Available)],
        })
        .expect("discover the studio Origin")
    else {
        panic!("the picker should list every discovered Origin");
    };
    for request in requests {
        application
            .handle_event(ApplicationEvent::SessionsListed {
                request,
                sessions: Vec::new(),
            })
            .expect("finish the initial merged listing");
    }

    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::SessionCreated(SessionCreated {
                session_id: SessionId::new(),
            }),
        })
        .expect("take a background catalog change")
    else {
        panic!("the picker should catch up the changed Origin");
    };
    assert_eq!(request.surface(), SessionListSurface::SessionPicker);
    assert_eq!(request.outlook(), &Outlook::Remote("studio".to_owned()));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: Vec::new(),
        })
        .expect("finish the catch-up");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("hide the Everywhere picker");

    let ApplicationTransition::ListSessions(hidden_request) = application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::SessionCreated(SessionCreated {
                session_id: SessionId::new(),
            }),
        })
        .expect("take a catalog change while the picker is hidden")
    else {
        panic!("chosen Everywhere scope keeps its hidden catalog current");
    };
    assert_eq!(hidden_request.surface(), SessionListSurface::SessionPicker);
    assert_eq!(
        hidden_request.outlook(),
        &Outlook::Remote("studio".to_owned())
    );
}

#[test]
fn a_terminal_remote_failure_does_not_touch_an_independently_scoped_picker() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    open_session_picker_with(
        &mut application,
        vec![session_summary(
            SessionId::new(),
            workspace.path(),
            "Keep local",
            SessionStatus::Idle,
            10,
        )],
    );

    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::RemoteFailed {
                status: RemoteStatus::Revoked,
                message: "Pairing revoked".to_owned(),
            },
        })
        .expect("end an unrelated Remote");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Keep local"),
        "an ended Remote removes only its own rows"
    );
}

#[test]
fn session_picker_switching_restores_each_transcript_viewport() {
    let workspace = workspace_dir();
    let first_id = SessionId::new();
    let first_snapshot = navigable_session_snapshot(first_id, workspace.path(), 8);
    let second_id = SessionId::new();
    let second_snapshot = navigable_session_snapshot(second_id, workspace.path(), 3);
    let mut application = Application::new(workspace.path(), Default::default());
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let request = expect_session_list_request(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("open Session picker"),
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            newest_id,
        ))
    );
}

#[test]
fn unreadable_session_picker_rows_remain_navigable_without_attachment() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
            workspace: Some(Workspace::directory(workspace.path().to_owned())),
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
    let workspace = workspace_dir();
    let current_id = SessionId::new();
    let mut application = Application::new(workspace.path(), Default::default());
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
        SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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

#[test]
fn session_picker_draws_an_emoji_beside_its_title_and_leaves_a_session_without_one_where_it_was() {
    let workspace = workspace_dir();
    let derived_id = SessionId::new();
    let underived_id = SessionId::new();
    let mut mixed = client_showing_emojis(workspace.path());
    open_session_picker_with(
        &mut mixed,
        vec![
            emoji_session_summary(
                derived_id,
                workspace.path(),
                "Reasoning group flicker fixed",
                "🚀",
                20,
            ),
            session_summary(
                underived_id,
                workspace.path(),
                "look at this stack trace",
                SessionStatus::Idle,
                10,
            ),
        ],
    );

    let rows = rendered_application_rows(&mixed);
    assert!(
        rows.iter()
            .find(|row| row.contains("Reasoning group flicker fixed"))
            .expect("render the Session with an Emoji")
            .contains('🚀'),
        "a Session with an Emoji draws it on its Title's row"
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.contains("look at this stack trace"))
            .expect("render the Session without an Emoji")
            .contains('🚀'),
        "an Emoji belongs to one Session's row and no other"
    );
    // Both rows begin at the same place: the box's own left edge, which the
    // search line above them starts at, plus the two columns every row spends
    // on its selection marker. An Emoji takes the front of the row it belongs
    // to, and the row without one holds no cell open where an Emoji would have
    // gone — it is drawn exactly as it was before Emoji existed.
    let mixed_buffer = rendered_application_buffer(&mixed, 80, 15);
    let first_column = text_position(&mixed_buffer, "Search:").0 + SESSION_ROW_MARKER_WIDTH;
    assert_eq!(
        text_position(&mixed_buffer, "🚀").0,
        first_column,
        "an Emoji draws at the front of its own row"
    );
    assert!(
        text_position(&mixed_buffer, "Reasoning group flicker fixed").0 > first_column,
        "the Title it stands for follows it"
    );
    assert_eq!(
        text_position(&mixed_buffer, "look at this").0,
        first_column,
        "a Session without an Emoji draws where it always did"
    );

    // The Emoji costs its own columns and no more, so a Title still reads at
    // the narrowest terminal the picker supports.
    let narrow = rendered_application_rows_at(&mixed, 28, 8);
    assert!(
        narrow
            .iter()
            .find(|row| row.contains("Reasoning"))
            .expect("render the Session with an Emoji on a narrow terminal")
            .contains('🚀')
    );
    assert!(narrow.join("\n").contains("look at this"));
}

/// Every Session picker row spends its first two columns on the marker naming
/// the selected row, so what a row draws of its own begins just past them.
const SESSION_ROW_MARKER_WIDTH: u16 = 2;

/// The Emoji beside a Session's Title is drawn in the picker only where the
/// reader asked for one, and a row left without it reads exactly as the row of
/// a Session that never had one.
#[test]
fn a_session_picker_row_draws_its_emoji_only_where_the_setting_shows_them() {
    let workspace = workspace_dir();
    let sessions = || {
        vec![emoji_session_summary(
            SessionId::new(),
            workspace.path(),
            "Reasoning group flicker fixed",
            "🚀",
            20,
        )]
    };

    let mut hidden = Application::new(workspace.path(), Default::default());
    open_session_picker_with(&mut hidden, sessions());
    let drawn = rendered_application_rows(&hidden);
    let row = drawn
        .iter()
        .find(|row| row.contains("Reasoning group flicker fixed"))
        .expect("render the Session")
        .clone();
    assert!(
        !row.contains('🚀'),
        "a Session's name carries no Emoji until the reader asks for one: {row:?}"
    );
    let buffer = rendered_application_buffer(&hidden, 80, 15);
    assert_eq!(
        text_position(&buffer, "Reasoning group flicker fixed").0,
        text_position(&buffer, "Search:").0 + SESSION_ROW_MARKER_WIDTH,
        "and its Title holds no cell open where an Emoji would have gone"
    );

    let mut shown = client_showing_emojis(workspace.path());
    open_session_picker_with(&mut shown, sessions());
    assert!(
        rendered_application_rows(&shown)
            .iter()
            .find(|row| row.contains("Reasoning group flicker fixed"))
            .expect("render the Session")
            .contains('🚀'),
        "and the Emoji leads the row once they have"
    );
}

#[test]
fn session_picker_search_matches_the_words_of_a_title_and_never_the_emoji_beside_it() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    open_session_picker_with(
        &mut application,
        vec![
            emoji_session_summary(
                SessionId::new(),
                workspace.path(),
                "Rocket telemetry parsed",
                "🚀",
                20,
            ),
            emoji_session_summary(
                SessionId::new(),
                workspace.path(),
                "Ledger reconciliation",
                "🧾",
                10,
            ),
        ],
    );

    type_terminal_text(&mut application, "rocket");
    let by_word = rendered_application_rows(&application).join("\n");
    assert!(by_word.contains("Rocket telemetry parsed"));
    assert!(!by_word.contains("Ledger reconciliation"));

    for _ in 0.."rocket".len() {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            )))
            .expect("clear the Session picker search");
    }
    type_terminal_text(&mut application, "🚀");
    let by_emoji = rendered_application_rows(&application).join("\n");
    assert!(
        by_emoji.contains("No Sessions found"),
        "an Emoji is carried beside a Title and is never matched against"
    );
    assert!(!by_emoji.contains("Rocket telemetry parsed"));
}

#[test]
fn an_emoji_arriving_while_the_picker_is_open_lands_on_its_row() {
    let workspace = workspace_dir();
    let mut application = client_showing_emojis(workspace.path());
    let derived_id = SessionId::new();
    open_session_picker_with(
        &mut application,
        vec![
            session_summary(
                derived_id,
                workspace.path(),
                "the reasoning group keeps flickering when",
                SessionStatus::Idle,
                20,
            ),
            session_summary(
                SessionId::new(),
                workspace.path(),
                "Untouched Session",
                SessionStatus::Idle,
                10,
            ),
        ],
    );

    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::SessionTitleChanged(SessionTitleChanged {
                session_id: derived_id,
                title: "Reasoning group flicker fixed".to_owned(),
                emoji: Some("🧵".to_owned()),
            }),
        ))
        .expect("apply a derived Title and Emoji");

    let retitled = rendered_application_rows(&application);
    let derived_row = retitled
        .iter()
        .find(|row| row.contains("Reasoning group flicker fixed"))
        .expect("redraw the retitled Session's row");
    assert!(
        derived_row.contains('🧵'),
        "an Emoji landing while the picker is open lands on its own row"
    );
    // In place: the row keeps the position and the focus it had, so a Title
    // landing under a reader's eyes never moves what they were about to open.
    assert!(derived_row.contains('›'));
    assert!(
        rendered_row(&retitled, "Reasoning group flicker fixed")
            < rendered_row(&retitled, "Untouched Session")
    );
    assert!(
        !retitled
            .join("\n")
            .contains("the reasoning group keeps flickering when")
    );
    assert!(
        !retitled
            .iter()
            .find(|row| row.contains("Untouched Session"))
            .expect("redraw the Session the derivation was not for")
            .contains('🧵')
    );
}

#[test]
fn an_emoji_leaves_an_all_workspaces_row_room_for_its_workspace_path() {
    let workspace = workspace_dir();
    // A second Workspace this test only names, rooted the way the running
    // platform roots one so the picker draws it back as spelled.
    let other_workspace = named_workspace_path("ws-two");
    let mut application = client_showing_emojis(workspace.path());
    open_session_picker_with(
        &mut application,
        vec![session_summary(
            SessionId::new(),
            workspace.path(),
            "Current Workspace Session",
            SessionStatus::Idle,
            10,
        )],
    );
    let all_workspaces = expect_session_list_request(
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
            request: all_workspaces,
            sessions: vec![emoji_session_summary(
                SessionId::new(),
                &other_workspace,
                "Ledger reconciliation",
                "🧾",
                20,
            )],
        })
        .expect("hydrate all-Workspace Sessions");

    let row = rendered_application_rows(&application)
        .into_iter()
        .find(|row| row.contains("Ledger reconciliation"))
        .expect("render an all-Workspaces row for a Session with an Emoji");
    assert!(row.contains('🧾'));
    assert!(
        row.contains(other_workspace.to_string_lossy().as_ref()),
        "an Emoji takes its columns from the Title rather than from the path \
         that tells one Workspace's Session from another's: {row:?}"
    );
}

fn session_summary(
    session_id: SessionId,
    workspace: &std::path::Path,
    title: &str,
    status: SessionStatus,
    updated_at: u64,
) -> SessionListItem {
    SessionListItem::Readable(Box::new(SessionSummary {
        checkout_state: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status,
            working_since: None,
            parent: None,
        },
        title: title.to_owned(),
        emoji: None,
        settled_at: None,
        standing_inputs: Default::default(),
        total_usage: None,
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(updated_at),
    }))
}

fn remote(name: &str, status: RemoteStatus) -> Remote {
    Remote {
        name: name.to_owned(),
        fingerprint: format!("{name}-fingerprint"),
        addresses: Vec::new(),
        status,
    }
}

/// A Session listing entry whose derivation already landed, so it carries an
/// Emoji beside its Title.
fn emoji_session_summary(
    session_id: SessionId,
    workspace: &std::path::Path,
    title: &str,
    emoji: &str,
    updated_at: u64,
) -> SessionListItem {
    match session_summary(
        session_id,
        workspace,
        title,
        SessionStatus::Idle,
        updated_at,
    ) {
        SessionListItem::Readable(summary) => SessionListItem::Readable(Box::new(SessionSummary {
            emoji: Some(emoji.to_owned()),
            ..*summary
        })),
        unreadable => unreadable,
    }
}

/// A client whose reader has turned Session name Emojis on, which is what a
/// test about a row that draws one asks for: a Session's name carries no Emoji
/// until the Setting says it does. The Sidebar is left down, so the picker is
/// the only thing on screen listing Sessions.
fn client_showing_emojis(workspace: &std::path::Path) -> Application {
    let mut application = Application::new(workspace, Default::default());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    session: SessionSettings {
                        title: TitleSettings {
                            emoji: EmojiVisibility::Shown,
                            ..TitleSettings::default()
                        },
                        ..SessionSettings::default()
                    },
                    sidebar: SidebarSettings {
                        initial_visibility: SidebarVisibility::Hidden,
                        ..SidebarSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the effective-settings snapshot");
    application
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
        SessionListScope::CurrentWorkspace((workspace).into()),
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
