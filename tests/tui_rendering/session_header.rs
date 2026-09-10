//! The viewed Session's Title stays centered between the header indicators.
use crate::support::{
    connected_application, enter_session, navigable_session_snapshot, rendered_application_rows_at,
    workspace_dir,
};
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        CheckoutAssociation, CheckoutId, CheckoutKind, CheckoutRevision, CheckoutStateChanged,
        CheckoutSummary, EffectiveSettings, EmojiVisibility, RepositoryId, SessionChange,
        SessionRevision, SessionSnapshot, SessionUpdate, SettingsSnapshot, SidebarVisibility,
        SourceControlAvailability,
    },
    tui::{Application, ApplicationEvent},
};

fn settings(application: &mut Application, emoji: EmojiVisibility) {
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.session.title.emoji = emoji;
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

fn enable_icons(application: &mut Application) {
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.appearance.show_icons = true;
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings,
                pinned: vec!["appearance.showIcons".to_owned()],
                diagnostics: Vec::new(),
            },
        )))
        .unwrap();
}

fn show(application: &mut Application, snapshot: &SessionSnapshot) {
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .unwrap();
}

fn header(application: &Application, width: u16) -> String {
    rendered_application_rows_at(application, width, 20)
        .into_iter()
        .find(|row| row.contains("Connected"))
        .expect("Session header")
}

fn application_with_checkout_branch(
    workspace: &std::path::Path,
    kind: CheckoutKind,
    branch: &str,
    child: bool,
) -> Application {
    let mut application = connected_application(workspace);
    settings(&mut application, EmojiVisibility::Hidden);
    let (parent, mut snapshot) = enter_session(&mut application, workspace);
    let repository = RepositoryId::from_metadata("git", workspace);
    let association = CheckoutAssociation {
        recovery_revision: None,
        id: CheckoutId::from_root(&repository, workspace),
        repository,
        root: workspace.to_owned(),
        kind,
    };
    snapshot.session.checkout = Some(association.clone());
    if child {
        snapshot.session.id = suru::protocol::SessionId::new();
        snapshot.session.parent = Some(parent);
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .unwrap();
    } else {
        show(&mut application, &snapshot);
    }
    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::CheckoutStateChanged(CheckoutStateChanged {
                checkout_id: association.id.clone(),
                checkout_state: Some(CheckoutSummary {
                    association,
                    revision: Some(CheckoutRevision::Branch {
                        name: branch.to_owned(),
                        commit: Some("0123456789abcdef".to_owned()),
                    }),
                    availability: SourceControlAvailability::Available,
                }),
            }),
        ))
        .unwrap();
    application
}

#[test]
fn branch_name_follows_the_workspace_name_in_the_session_header() {
    let workspace = workspace_dir();
    let application = application_with_checkout_branch(
        workspace.path(),
        CheckoutKind::Main,
        "feature/header-context",
        false,
    );

    let row = header(&application, 240);
    assert!(
        row.contains(&format!(
            "{} · feature/header-context",
            workspace.path().file_name().unwrap().to_string_lossy()
        )),
        "{row}"
    );
    assert!(
        !row.contains(workspace.path().to_string_lossy().as_ref()),
        "the header names the Workspace without showing its full path: {row}"
    );
    let narrow = header(&application, 40);
    assert!(
        !narrow.contains("feature/header-context") && !narrow.trim_start().starts_with('·'),
        "a hidden workspace path leaves no branch or orphan separator: {narrow}"
    );
}

#[test]
fn icons_identify_the_workspace_and_worktree_in_the_session_header() {
    let workspace = workspace_dir();
    for (kind, icon, legacy_suffix) in [
        (CheckoutKind::Main, '\u{ec6f}', ""),
        (CheckoutKind::Linked, '\u{ec7e}', " (worktree)"),
    ] {
        let mut application =
            application_with_checkout_branch(workspace.path(), kind, "feature/icons", false);
        enable_icons(&mut application);

        let row = header(&application, 240);
        assert!(
            row.contains(&format!(
                "\u{ea83} {} · {icon} feature/icons",
                workspace.path().file_name().unwrap().to_string_lossy()
            )),
            "{row}"
        );
        assert!(!row.contains("(worktree)"), "{row}");
        let narrow = header(&application, 40);
        assert!(
            !narrow.contains('\u{ea83}') && !narrow.contains(icon),
            "icons disappear with their labels at narrow widths: {narrow}"
        );

        settings(&mut application, EmojiVisibility::Hidden);
        let plain = header(&application, 240);
        assert!(
            plain.contains(&format!(" · feature/icons{legacy_suffix}")),
            "{plain}"
        );
        assert!(
            !plain.contains('\u{ea83}') && !plain.contains(icon),
            "{plain}"
        );
    }
}

#[test]
fn remote_header_icons_preserve_the_centered_title_and_compact_remote_label() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    enable_icons(&mut application);
    crate::connecting::turn_to_studio(&mut application);
    let mut snapshot =
        navigable_session_snapshot(suru::protocol::SessionId::new(), workspace.path(), 1);
    snapshot.title = "Alpha".into();
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();

    let row = header(&application, 240);
    assert!(row.contains("\u{f0379} studio · \u{ea83} "), "{row}");
    assert_eq!(
        row.find("Alpha")
            .map(|offset| row[..offset].chars().count()),
        Some((240 - 5) / 2),
        "{row}"
    );
    for width in [28, 40] {
        let row = header(&application, width);
        assert!(row.contains("\u{f0379} studio"), "width {width}: {row}");
        assert!(!row.contains('\u{ea83}'), "width {width}: {row}");
        assert!(row.contains("Connected"), "width {width}: {row}");
    }
}

#[test]
fn main_branch_is_shown_in_the_session_header() {
    let workspace = workspace_dir();
    let application =
        application_with_checkout_branch(workspace.path(), CheckoutKind::Main, "main", false);

    let row = header(&application, 240);
    assert!(
        row.contains(&format!(
            "{} · main",
            workspace.path().file_name().unwrap().to_string_lossy()
        )),
        "{row}"
    );

    let linked =
        application_with_checkout_branch(workspace.path(), CheckoutKind::Linked, "main", false);
    let row = header(&linked, 240);
    assert!(row.contains(" · main (worktree)"), "{row}");
}

#[test]
fn linked_worktree_label_follows_the_branch_name_in_the_session_header() {
    let workspace = workspace_dir();
    let application = application_with_checkout_branch(
        workspace.path(),
        CheckoutKind::Linked,
        "feature/worktree-context",
        false,
    );

    let row = header(&application, 240);
    assert!(
        row.contains(&format!(
            "{} · feature/worktree-context (worktree)",
            workspace.path().file_name().unwrap().to_string_lossy()
        )),
        "{row}"
    );
}

#[test]
fn viewed_child_uses_the_branch_state_shared_by_its_worktree() {
    let workspace = workspace_dir();
    let application = application_with_checkout_branch(
        workspace.path(),
        CheckoutKind::Linked,
        "feature/child-context",
        true,
    );

    let row = header(&application, 240);
    assert!(
        row.contains(&format!(
            "{} · feature/child-context (worktree)",
            workspace.path().file_name().unwrap().to_string_lossy()
        )),
        "{row}"
    );
}

#[test]
fn title_is_centered_in_the_view_and_updates_with_the_session() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    settings(&mut application, EmojiVisibility::Hidden);
    let (_, mut snapshot) = enter_session(&mut application, workspace.path());
    snapshot.title = "Alpha".into();
    show(&mut application, &snapshot);
    for width in [160, 200] {
        let row = header(&application, width);
        assert_eq!(
            row.find("Alpha")
                .map(|offset| row[..offset].chars().count()),
            Some((usize::from(width) - 5) / 2),
            "{row}"
        );
        assert!(
            row.contains(
                workspace
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            ) && row.contains("Connected"),
            "{row}"
        );
        assert!(!row.contains("Workspace") && !row.contains("Suru"), "{row}");
    }
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id: snapshot.session.id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::TitleChanged {
                    title: "Bravo".into(),
                    emoji: Some("🐛".into()),
                }],
            },
        )))
        .unwrap();
    let row = header(&application, 160);
    assert!(
        row.contains("Bravo") && !row.contains("Alpha") && !row.contains("🐛"),
        "{row}"
    );
    settings(&mut application, EmojiVisibility::Shown);
    assert!(header(&application, 160).contains("🐛"));
}

#[test]
fn long_multiline_titles_shrink_without_overwriting_indicators() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    settings(&mut application, EmojiVisibility::Hidden);
    let (_, mut snapshot) = enter_session(&mut application, workspace.path());
    snapshot.title = "Alpha\n\tBeta".into();
    show(&mut application, &snapshot);
    assert!(header(&application, 200).contains("Alpha Beta"));
    snapshot.title = "界".repeat(200);
    show(&mut application, &snapshot);
    let wide = header(&application, 200);
    assert!(
        wide.contains("界") && wide.contains('…') && wide.contains("Connected"),
        "{wide}"
    );
    for width in 28..100 {
        let row = header(&application, width);
        assert!(row.contains("Connected"), "width {width}: {row}");
        snapshot.title.clear();
        show(&mut application, &snapshot);
        let empty = header(&application, width);
        // Every cell belonging to either existing indicator keeps its content.
        let prefix = empty
            .trim_end()
            .split("Connected")
            .next()
            .unwrap()
            .trim_end();
        assert!(row.starts_with(prefix), "width {width}: {row}");
        snapshot.title = "界".repeat(200);
        show(&mut application, &snapshot);
    }
}

#[test]
fn viewed_child_uses_its_own_title_and_empty_title_leaves_the_center_blank() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    settings(&mut application, EmojiVisibility::Shown);
    let (parent, mut snapshot) = enter_session(&mut application, workspace.path());
    snapshot.session.id = suru::protocol::SessionId::new();
    snapshot.session.parent = Some(parent);
    snapshot.title = "Child work".into();
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .unwrap();
    assert!(header(&application, 200).contains("Child work"));
    snapshot.title.clear();
    snapshot.emoji = Some("🐛".into());
    show(&mut application, &snapshot);
    let row = header(&application, 200);
    assert!(!row.contains("Child work") && !row.contains("🐛"), "{row}");
}
