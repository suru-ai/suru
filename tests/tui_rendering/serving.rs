//! The Serving user's `/serve` command and overlay.

use std::net::{Ipv4Addr, SocketAddr};

use crate::support::{
    deliver_settings, rendered_application_rows, rendered_application_rows_at, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    protocol::{EffectiveSettings, IssueInviteRequest, IssuedInvite, Peer, SettingsSnapshot},
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
};

fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    press_with(application, code, KeyModifiers::NONE)
}

fn press_with(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("handle Serving overlay key")
}

#[test]
fn serving_shows_a_fresh_copyable_invite_and_removes_enrolled_peers() {
    let mut application = Application::default();
    let mut settings = EffectiveSettings::default();
    settings.serving.enabled = true;
    settings.serving.port = 7777;
    deliver_settings(&mut application, settings);

    type_terminal_text(&mut application, "/serve");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginServing {
            enable: false,
            port: 7777,
        }
    );
    application
        .handle_event(ApplicationEvent::ServingPrepared {
            settings: None,
            candidates: vec![SocketAddr::from((Ipv4Addr::new(10, 0, 0, 8), 7777))],
        })
        .expect("load Serving candidates");
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::IssueInvite(_)
    ));

    let invite = "suru-v1-example".to_owned();
    application
        .handle_event(ApplicationEvent::InviteIssued {
            invite: IssuedInvite {
                invite: invite.clone(),
                addresses: vec![SocketAddr::from((Ipv4Addr::new(10, 0, 0, 8), 7777))],
            },
            peers: vec![Peer {
                id: "peer-laptop".to_owned(),
                fingerprint: "laptop-fingerprint".to_owned(),
            }],
        })
        .expect("show fresh Invite and Peers");

    let management = rendered_application_rows(&application).join("\n");
    assert!(management.contains("Fresh Invite"));
    assert!(management.contains(&invite));
    assert!(management.contains("Enrolled Peers"));
    assert!(management.contains("laptop-fingerprint"));
    assert!(management.contains("Ctrl+C copy"));

    assert_eq!(
        press_with(&mut application, KeyCode::Char('c'), KeyModifiers::CONTROL,),
        ApplicationTransition::CopyToClipboard(invite.into())
    );
    // Removing a Peer ends a Pairing, so it is asked for twice: the first
    // press arms it and any other key puts it down again.
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal · any other key cancels")
    );
    assert_eq!(
        press(&mut application, KeyCode::Down),
        ApplicationTransition::Continue
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("x confirm removal")
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::RemovePeer("peer-laptop".to_owned())
    );
    application
        .handle_event(ApplicationEvent::PeerRemoved("peer-laptop".to_owned()))
        .expect("remove Peer from the overlay");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("laptop-fingerprint")
    );
}

#[test]
fn serve_enables_serving_and_invites_only_the_selected_candidate_addresses() {
    let mut application = Application::default();
    let mut settings = EffectiveSettings::default();
    settings.serving.port = 7443;
    deliver_settings(&mut application, settings.clone());

    type_terminal_text(&mut application, "/serve");
    let completion = rendered_application_rows(&application).join("\n");
    assert!(completion.contains("/serve"));
    assert!(completion.contains("Serve this machine"));
    assert_eq!(SemanticCommandId::ServeOpen.as_str(), "serve.open");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginServing {
            enable: true,
            port: 7443,
        }
    );

    settings.serving.enabled = true;
    application
        .handle_event(ApplicationEvent::ServingPrepared {
            settings: Some(SettingsSnapshot {
                settings,
                pinned: vec!["serving.enabled".to_owned()],
                diagnostics: Vec::new(),
            }),
            candidates: vec![
                SocketAddr::from((Ipv4Addr::new(10, 0, 0, 8), 7443)),
                SocketAddr::from((Ipv4Addr::new(192, 168, 1, 24), 7443)),
            ],
        })
        .expect("load Serving candidates");

    let addresses = rendered_application_rows(&application).join("\n");
    assert!(addresses.contains("Choose Invite addresses"));
    assert!(addresses.contains("[x] 10.0.0.8:7443"));
    assert!(addresses.contains("[x] 192.168.1.24:7443"));

    assert_eq!(
        press(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::IssueInvite(IssueInviteRequest {
            addresses: vec![SocketAddr::from((Ipv4Addr::new(192, 168, 1, 24), 7443))],
        })
    );
}

#[test]
fn serve_lists_scroll_to_keep_the_focused_candidate_and_peer_fully_visible() {
    let mut application = Application::default();
    let mut settings = EffectiveSettings::default();
    settings.serving.enabled = true;
    settings.serving.port = 7777;
    deliver_settings(&mut application, settings);
    type_terminal_text(&mut application, "/serve");
    press(&mut application, KeyCode::Enter);

    let candidates = (1..=20)
        .map(|last| SocketAddr::from((Ipv4Addr::new(10, 0, 0, last), 7777)))
        .collect::<Vec<_>>();
    application
        .handle_event(ApplicationEvent::ServingPrepared {
            settings: None,
            candidates,
        })
        .expect("load more candidates than one frame holds");
    for _ in 1..20 {
        press(&mut application, KeyCode::Down);
    }
    assert!(
        rendered_application_rows_at(&application, 60, 15)
            .join("\n")
            .contains("10.0.0.20:7777")
    );
    press(&mut application, KeyCode::Enter);

    let peers = (0..10)
        .map(|index| Peer {
            id: format!("peer-{index}"),
            fingerprint: format!("{index:02}{}", "f".repeat(62)),
        })
        .collect::<Vec<_>>();
    let long_invite = format!("suru-v1-{}", "a".repeat(150));
    application
        .handle_event(ApplicationEvent::InviteIssued {
            invite: IssuedInvite {
                invite: long_invite.clone(),
                addresses: vec![SocketAddr::from((Ipv4Addr::new(10, 0, 0, 20), 7777))],
            },
            peers,
        })
        .expect("show more Peers than one frame holds");
    for _ in 1..10 {
        press(&mut application, KeyCode::Down);
    }
    let rows = rendered_application_rows_at(&application, 60, 24);
    let invite_start = rows
        .iter()
        .position(|row| row.contains("Fresh Invite"))
        .expect("Invite heading is visible")
        + 1;
    let invite_end = rows
        .iter()
        .position(|row| row.contains("Enrolled Peers"))
        .expect("Peer heading follows the complete Invite");
    let selectable_invite = rows[invite_start..invite_end]
        .iter()
        .map(|row| row.trim().trim_matches('│').trim())
        .collect::<String>();
    assert_eq!(selectable_invite, long_invite);
    assert!(
        rows.iter()
            .any(|row| row.contains("09") && row.contains(&"f".repeat(50))),
        "the focused Peer's first wrapped row should be visible: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.contains(&"f".repeat(10))),
        "the focused Peer's wrapped suffix should remain visible: {rows:#?}"
    );
}
