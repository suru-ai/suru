//! The viewed Session's Title stays centered between the header indicators.
use crate::support::{
    connected_application, enter_session, rendered_application_rows_at, workspace_dir,
};
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        EffectiveSettings, EmojiVisibility, SessionChange, SessionRevision, SessionSnapshot,
        SessionUpdate, SettingsSnapshot, SidebarVisibility,
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
        assert!(row.contains("Connected"), "{row}");
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
