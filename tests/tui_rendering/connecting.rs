//! The connecting user's `/connect` command and Pairing surfaces.

use crate::support::{rendered_application_rows, type_terminal_text};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    protocol::{InvitePreview, RedeemInviteRequest, Remote, RemoteHealth, RemoteStatus},
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
};

fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    press_with(application, code, KeyModifiers::NONE)
}

#[test]
fn paired_remote_picker_shows_each_pairing_status() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    let remote = |name: &str| Remote {
        name: name.to_owned(),
        fingerprint: format!("{name}-fingerprint"),
        addresses: vec!["10.0.0.8:7777".parse().unwrap()],
    };
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            remote("studio"),
            remote("old"),
            remote("offline"),
        ]))
        .unwrap();
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Checking…")
    );

    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: 28,
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "old".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: 27,
                status: RemoteStatus::ProtocolMismatch,
            }),
        })
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "offline".to_owned(),
            result: Err("could not reach Remote".to_owned()),
        })
        .unwrap();

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("studio  Available"));
    assert!(picker.contains("old  Protocol v27 mismatch"));
    assert!(picker.contains("offline  Unavailable · could not reach Remote"));
}

#[test]
fn pairing_another_remote_refuses_a_duplicate_prefilled_name_in_the_draft() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "known".to_owned(),
            addresses: vec!["10.0.0.4:7777".parse().unwrap()],
        }]))
        .unwrap();

    assert_eq!(
        press(&mut application, KeyCode::Char('a')),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Paste Invite")
    );
    application
        .handle_terminal_event(InputEvent::Paste("suru-v1-another".to_owned()))
        .unwrap();
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            invite: "suru-v1-another".to_owned(),
            preview: InvitePreview {
                hostname: "studio".to_owned(),
                fingerprint: "new".to_owned(),
                addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            },
        })
        .unwrap();
    press(&mut application, KeyCode::Enter);

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("A Remote named `studio` already exists")
    );
}

fn press_with(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("handle Connect overlay key")
}

fn invite_entry() -> Application {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(Vec::new()))
        .unwrap();
    application
}

fn redemption_in_flight() -> Application {
    let mut application = invite_entry();
    application
        .handle_terminal_event(InputEvent::Paste("suru-v1-example".to_owned()))
        .unwrap();
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            invite: "suru-v1-example".to_owned(),
            preview: InvitePreview {
                hostname: "studio".to_owned(),
                fingerprint: "fingerprint".to_owned(),
                addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            },
        })
        .unwrap();
    press(&mut application, KeyCode::Enter);
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::RedeemInvite(_)
    ));
    application
}

#[test]
fn every_invite_refusal_is_precise_and_visible_on_the_step_that_failed() {
    for (invite, error) in [
        ("not-an-invite", "Invite is malformed"),
        ("suru-v2-e30", "Invite version `v2` is not supported"),
    ] {
        let mut application = invite_entry();
        application
            .handle_terminal_event(InputEvent::Paste(invite.to_owned()))
            .unwrap();
        press(&mut application, KeyCode::Enter);
        application
            .handle_event(ApplicationEvent::InvitePreviewFailed {
                invite: invite.to_owned(),
                error: error.to_owned(),
            })
            .unwrap();
        assert!(
            rendered_application_rows(&application)
                .join("\n")
                .contains(error)
        );
    }

    for error in ["Invite has expired", "Invite has already been spent"] {
        let mut application = redemption_in_flight();
        application
            .handle_event(ApplicationEvent::InviteRedemptionFailed(error.to_owned()))
            .unwrap();
        let details = rendered_application_rows(&application).join("\n");
        assert!(details.contains("Configure Remote"));
        assert!(details.contains(error));
    }
}

#[test]
fn successful_redemption_opens_the_paired_remote_picker() {
    let mut application = redemption_in_flight();
    application
        .handle_event(ApplicationEvent::RemoteRedeemed(Remote {
            name: "studio".to_owned(),
            fingerprint: "fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
        }))
        .unwrap();

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Paired Remotes"));
    assert!(picker.contains("studio  Available"));
}

#[test]
fn pairing_another_remote_keeps_every_paired_remote_in_the_picker() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.4:7777".parse().unwrap()],
        }]))
        .unwrap();
    press(&mut application, KeyCode::Char('a'));
    application
        .handle_terminal_event(InputEvent::Paste("suru-v1-another".to_owned()))
        .unwrap();
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            invite: "suru-v1-another".to_owned(),
            preview: InvitePreview {
                hostname: "laptop".to_owned(),
                fingerprint: "laptop-fingerprint".to_owned(),
                addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            },
        })
        .unwrap();
    press(&mut application, KeyCode::Enter);
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::RedeemInvite(_)
    ));
    application
        .handle_event(ApplicationEvent::RemoteRedeemed(Remote {
            name: "laptop".to_owned(),
            fingerprint: "laptop-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
        }))
        .unwrap();

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("studio  Checking…"));
    assert!(picker.contains("laptop  Available"));
}

#[test]
fn pasted_invite_shows_its_fingerprint_before_pairing_can_advance() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(Vec::new()))
        .unwrap();

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Paste("suru-v1-example".to_owned()))
            .expect("paste Invite"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::PreviewInvite("suru-v1-example".to_owned())
    );
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            invite: "suru-v1-example".to_owned(),
            preview: InvitePreview {
                hostname: "studio".to_owned(),
                fingerprint: "0123456789abcdef".repeat(4),
                addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            },
        })
        .expect("show Invite fingerprint");

    let confirmation = rendered_application_rows(&application).join("\n");
    assert!(confirmation.contains("Confirm Serving Server"));
    assert!(confirmation.contains(&"0123456789abcdef".repeat(4)));
    assert!(confirmation.contains("Enter trust"));
    assert!(!confirmation.contains("Remote name"));

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    let details = rendered_application_rows(&application).join("\n");
    assert!(details.contains("Remote name"));
    assert!(details.contains("studio"));
}

#[test]
fn remote_name_is_editable_and_addresses_are_redeemed_in_the_visible_priority_order() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(Vec::new()))
        .unwrap();
    application
        .handle_terminal_event(InputEvent::Paste("suru-v1-example".to_owned()))
        .unwrap();
    press(&mut application, KeyCode::Enter);
    let first = "10.0.0.8:7777".parse().unwrap();
    let preferred = "192.168.1.24:7777".parse().unwrap();
    application
        .handle_event(ApplicationEvent::InvitePreviewed {
            invite: "suru-v1-example".to_owned(),
            preview: InvitePreview {
                hostname: "studio".to_owned(),
                fingerprint: "fingerprint".to_owned(),
                addresses: vec![first, preferred],
            },
        })
        .unwrap();
    press(&mut application, KeyCode::Enter);

    for _ in 0.."studio".len() {
        press(&mut application, KeyCode::Backspace);
    }
    for character in "desktop".chars() {
        press(&mut application, KeyCode::Char(character));
    }
    press(&mut application, KeyCode::Tab);
    press(&mut application, KeyCode::Down);
    press_with(&mut application, KeyCode::Up, KeyModifiers::SHIFT);

    let draft = rendered_application_rows(&application).join("\n");
    assert!(draft.contains("> desktop"));
    assert!(draft.contains("1. 192.168.1.24:7777"));
    assert!(draft.contains("2. 10.0.0.8:7777"));
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::RedeemInvite(RedeemInviteRequest {
            invite: "suru-v1-example".to_owned(),
            name: Some("desktop".to_owned()),
            addresses: vec![preferred, first],
        })
    );
}

#[test]
fn connect_is_semantic_and_an_empty_remote_listing_opens_invite_entry() {
    let mut application = Application::default();

    type_terminal_text(&mut application, "/connect");
    let completion = rendered_application_rows(&application).join("\n");
    assert!(completion.contains("/connect"));
    assert!(completion.contains("Pair or choose a Remote"));
    assert_eq!(SemanticCommandId::ConnectOpen.as_str(), "connect.open");
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::BeginConnecting
    );

    application
        .handle_event(ApplicationEvent::RemotesListed(Vec::new()))
        .expect("open Invite entry for an empty Remote listing");

    let entry = rendered_application_rows(&application).join("\n");
    assert!(entry.contains("Paste Invite"));
    assert!(entry.contains("Enter inspect"));
}
