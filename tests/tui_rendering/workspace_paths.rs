//! Workspace labels use the owning Server's home without changing its paths.

use crate::support::{
    connected_application, enter_session, fixture_instance_id, ready_health,
    rendered_application_rows_at,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{EffectiveSettings, SettingsSnapshot, SidebarVisibility, WorkspacePaths},
    tui::{Application, ApplicationEvent},
};

fn hide_sidebar(application: &mut Application) {
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings,
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .unwrap();
}

fn screen(application: &Application) -> String {
    rendered_application_rows_at(application, 200, 30).join("\n")
}

#[test]
fn local_landing_uses_the_servers_home_while_a_session_uses_the_workspace_name() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("Projects").join("suru");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = suru::paths::canonical(&workspace).unwrap();
    let mut application = connected_application(&workspace);
    hide_sidebar(&mut application);
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42).with_workspace_paths(WorkspacePaths {
                home: Some(
                    suru::paths::canonical(home.path())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .into(),
                ),
                ..WorkspacePaths::default()
            }),
        )))
        .unwrap();

    let label = if cfg!(windows) {
        r"~\Projects\suru"
    } else {
        "~/Projects/suru"
    };
    let landing = screen(&application);
    let rows = landing.lines().collect::<Vec<_>>();
    let composer_bottom = rows.iter().position(|row| row.contains('└')).unwrap();
    assert_eq!(rows[composer_bottom + 1].trim(), label, "{landing}");
    assert!(!rows.last().unwrap().contains(label), "{landing}");
    assert!(!landing.contains("Workspace "), "{landing}");

    let (_, snapshot) = enter_session(&mut application, &workspace);
    let session = screen(&application);
    assert!(
        session.lines().next().unwrap().contains("suru"),
        "{session}"
    );
    assert!(
        !session.lines().next().unwrap().contains(label),
        "{session}"
    );
    assert!(!session.contains("Suru"), "{session}");
    assert!(!session.contains("Workspace "), "{session}");
    assert_eq!(snapshot.session.workspace.path, workspace);
}

#[test]
fn remote_landing_paths_and_session_names_follow_the_remote_path_style() {
    use std::path::Path;
    use suru::protocol::{
        Outlook, PathStyle, SessionCatalogRevision, SessionCatalogSnapshot, SessionId,
    };

    // These are wire paths, deliberately exercising both remote platforms on
    // every host; none is resolved using the test machine's filesystem.
    for (style, home, path, expected, expected_name) in [
        (
            PathStyle::Unix,
            Some("/home/remote"),
            "/home/remote/Projects/suru",
            "~/Projects/suru",
            "suru",
        ),
        (
            PathStyle::Windows,
            Some(r"C:\Users\Remote"),
            r"\\?\C:\Users\Remote\Projects\suru",
            r"~\Projects\suru",
            "suru",
        ),
        (
            PathStyle::Windows,
            Some(r"\\?\UNC\host\users\Remote"),
            r"\\host\users\Remote\suru",
            r"~\suru",
            "suru",
        ),
        (
            PathStyle::Windows,
            None,
            r"\\?\UNC\host\users\Remote\suru",
            r"\\host\users\Remote\suru",
            "suru",
        ),
        (
            PathStyle::Unix,
            Some("/home/remote"),
            "/home/remote",
            "~",
            "remote",
        ),
        (
            PathStyle::Unix,
            Some("/home/remote"),
            "/home/remote-other/suru",
            "/home/remote-other/suru",
            "suru",
        ),
        (
            PathStyle::Windows,
            Some(r"C:\Users\Remote"),
            r"\\?\D:\Projects\suru",
            r"D:\Projects\suru",
            "suru",
        ),
        (
            PathStyle::Unix,
            Some("/"),
            "/Projects/suru",
            "~/Projects/suru",
            "suru",
        ),
        (
            PathStyle::Windows,
            Some(r"C:\"),
            r"C:\Projects\suru",
            r"~\Projects\suru",
            "suru",
        ),
        (
            PathStyle::Unix,
            Some("/home/remote"),
            "/home/remote/../elsewhere/suru",
            "/home/remote/../elsewhere/suru",
            "suru",
        ),
    ] {
        let mut application = Application::default();
        hide_sidebar(&mut application);
        // The local home deliberately matches the remote path. It must never
        // supply an abbreviation when the remote home is unknown or differs.
        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
                ready_health(fixture_instance_id(), 42).with_workspace_paths(WorkspacePaths {
                    home: Some(path.into()),
                    style,
                }),
            )))
            .unwrap();
        crate::connecting::turn_to_studio(&mut application);
        let session_id = SessionId::new();
        application
            .handle_event(ApplicationEvent::OriginCatalog {
                outlook: Outlook::Remote("studio".into()),
                event: ManagedEvent::SessionCatalogReconciled(SessionCatalogSnapshot {
                    workspace_paths: WorkspacePaths {
                        home: home.map(str::to_owned),
                        style,
                    },
                    revision: SessionCatalogRevision::INITIAL,
                    session_ids: vec![session_id],
                    checkout_states: Vec::new(),
                }),
            })
            .unwrap();
        application
            .handle_event(ApplicationEvent::SessionAttached(
                crate::support::navigable_session_snapshot(session_id, Path::new(path), 1),
            ))
            .unwrap();
        let rendered = screen(&application);
        assert!(
            rendered.contains(&format!("studio · {expected_name} ")),
            "{path}: {rendered}"
        );
        assert!(
            !rendered.contains(&format!("studio · {expected} ")),
            "{rendered}"
        );
        assert!(
            !rendered.contains(&format!("Workspace {expected}")),
            "{rendered}"
        );
    }
}

#[test]
fn both_pickers_use_the_remote_home_even_without_a_remote_badge() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use suru::protocol::{
        Outlook, PathStyle, SessionCatalogRevision, SessionCatalogSnapshot, SessionId,
        SessionListItem, SessionSummary, SessionTimestamp,
    };
    use suru::tui::{ApplicationTransition, CommandId, SemanticCommandId};

    for (style, home, path, label, name) in [
        (
            PathStyle::Windows,
            r"C:\Users\Remote",
            r"C:\Users\Remote\suru",
            r"~\suru",
            "suru",
        ),
        // A backslash belongs to the Unix filename, even on a Windows Client.
        (
            PathStyle::Unix,
            "/home/Remote",
            r"/home/Remote/suru\notes",
            r"~/suru\notes",
            r"suru\notes",
        ),
    ] {
        for command in [
            SemanticCommandId::WorkspaceList,
            SemanticCommandId::SessionList,
        ] {
            let mut application = Application::default();
            hide_sidebar(&mut application);
            crate::connecting::turn_to_studio(&mut application);
            let id = SessionId::new();
            application
                .handle_event(ApplicationEvent::OriginCatalog {
                    outlook: Outlook::Remote("studio".into()),
                    event: ManagedEvent::SessionCatalogReconciled(SessionCatalogSnapshot {
                        workspace_paths: WorkspacePaths {
                            home: Some(home.into()),
                            style,
                        },
                        revision: SessionCatalogRevision::INITIAL,
                        session_ids: vec![id],
                        checkout_states: Vec::new(),
                    }),
                })
                .unwrap();
            let mut transition = application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    command,
                )))
                .unwrap();
            if command == SemanticCommandId::SessionList {
                transition = application
                    .handle_terminal_event(Event::Key(KeyEvent::new(
                        KeyCode::Char('a'),
                        KeyModifiers::CONTROL,
                    )))
                    .unwrap();
            }
            let ApplicationTransition::ListSessions(request) = transition else {
                panic!("picker requests Sessions")
            };
            let snapshot =
                crate::support::navigable_session_snapshot(id, std::path::Path::new(path), 1);
            application
                .handle_event(ApplicationEvent::SessionsListed {
                    request,
                    sessions: vec![SessionListItem::Readable(Box::new(SessionSummary {
                        checkout_state: None,
                        session: snapshot.session,
                        title: "Remote work".into(),
                        icon: None,
                        settled_at: None,
                        standing_inputs: Default::default(),
                        total_usage: None,
                        created_at: SessionTimestamp(1),
                        updated_at: SessionTimestamp(2),
                    }))],
                })
                .unwrap();
            let rendered = screen(&application);
            assert!(rendered.contains(label), "{command:?}: {rendered}");
            assert!(!rendered.contains(path), "{command:?}: {rendered}");
            if command == SemanticCommandId::WorkspaceList {
                crate::support::type_terminal_text(&mut application, "Remote");
                assert!(
                    !screen(&application).contains(label),
                    "Workspace search matches its name, not its home directory"
                );
                for _ in "Remote".chars() {
                    application
                        .handle_terminal_event(Event::Key(KeyEvent::new(
                            KeyCode::Backspace,
                            KeyModifiers::NONE,
                        )))
                        .unwrap();
                }
                application
                    .handle_terminal_event(Event::Key(KeyEvent::new(
                        KeyCode::Esc,
                        KeyModifiers::NONE,
                    )))
                    .unwrap();
                application
                    .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                        SemanticCommandId::SidebarToggle,
                    )))
                    .unwrap();
                crate::support::press_add_workspace(&mut application);
                crate::support::type_terminal_text(&mut application, path);
                let ApplicationTransition::ResolveWorkspace {
                    outlook,
                    surface,
                    request_id,
                    request,
                } = application
                    .handle_terminal_event(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    )))
                    .unwrap()
                else {
                    panic!("choosing a Workspace asks its Server to resolve it")
                };
                assert_eq!(request.path, std::path::Path::new(path));
                application
                    .handle_event(ApplicationEvent::WorkspaceResolved {
                        outlook,
                        surface,
                        request_id,
                        result: Ok(suru::protocol::ResolvedWorkspace::directory(request.path)),
                    })
                    .unwrap();
                let rows =
                    rendered_application_rows_at(&application, crate::support::SIDEBAR_WIDE, 30);
                assert_eq!(crate::support::selector_label(&rows), format!("▸ {name}"));
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn symlinked_homes_shorten_resolved_workspaces_but_links_outside_home_stay_absolute() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(home.join("suru")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let alias = root.path().join("home-alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("linked")).unwrap();
    let paths = WorkspacePaths::from_home(Some(&alias));
    // The Landing's line is only as wide as its composer, so a long temporary
    // path gives up its front. A link outside home is known by the end it
    // keeps — its resolved target's — and by nothing of it being spelled from
    // home.
    let resolved_outside = suru::paths::canonical(&outside).unwrap();
    let resolved_outside_end = resolved_outside
        .strip_prefix(resolved_outside.parent().unwrap().parent().unwrap())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    for (workspace, expected, spelled_from_home) in [
        (alias.join("suru"), "~/suru".to_owned(), true),
        (alias.join("linked"), resolved_outside_end, false),
    ] {
        let mut application = connected_application(&workspace);
        hide_sidebar(&mut application);
        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
                ready_health(fixture_instance_id(), 42).with_workspace_paths(paths.clone()),
            )))
            .unwrap();
        let rendered = screen(&application);
        assert!(rendered.contains(&format!("{expected} ")), "{rendered}");
        assert_eq!(rendered.contains("~/"), spelled_from_home, "{rendered}");
        assert!(
            !rendered.contains(&format!("Workspace {expected}")),
            "{rendered}"
        );
    }
}
