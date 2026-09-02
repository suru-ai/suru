//! The connecting user's `/connect` command and Pairing surfaces.

use std::collections::HashSet;

use crate::support::{
    fixture_instance_id, model_descriptor, navigable_session_snapshot, ready_health,
    rendered_application_rows, rendered_application_rows_at, text_on, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, InvitePreview, ModelAvailability, ModelCatalog, ModelId, Outlook,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, RedeemInviteRequest, Remote,
        RemoteHealth, RemoteStatus, Session, SessionCreated, SessionId, SessionListItem,
        SessionReference, SessionStatus, SessionSummary, SessionTimestamp, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        WorkspaceResolutionSurface,
    },
};

#[test]
fn choosing_a_remote_turns_the_outlook_and_the_footer_names_it() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Local"));
    assert!(picker.contains("studio  Available"));

    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote("studio".to_owned()),
            catalog_outlooks: HashSet::from([Outlook::Remote("studio".to_owned())]),
        }
    );

    let landing = rendered_application_rows(&application).join("\n");
    assert!(!landing.contains("Paired Remotes"));
    assert!(landing.contains("Outlook studio"));

    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), std::path::Path::new("."), 1),
        ))
        .unwrap();
    let session = rendered_application_rows(&application).join("\n");
    assert!(session.contains("Outlook studio"));

    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .unwrap()
    else {
        panic!("opening the Model picker asks the Remote for its Models");
    };
    assert_eq!(request.outlook(), &Outlook::Remote("studio".to_owned()));
}

#[test]
fn a_transient_remote_drop_reconnects_over_the_existing_view_and_preserves_its_composer() {
    let mut application = application_looking_at_studio();
    let session_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(session_id, std::path::Path::new("."), 1),
        ))
        .unwrap();
    type_terminal_text(&mut application, "unfinished thought");

    application
        .handle_event(ApplicationEvent::OutlookCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::Recovering(suru::managed_client::RecoveryStatus {
                attempt: 1,
                retry_in: std::time::Duration::from_millis(5),
            }),
        })
        .unwrap();
    application
        .handle_event(ApplicationEvent::ReconnectGraceElapsed)
        .unwrap();

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Reconnecting to Suru…")
    );

    application
        .handle_event(ApplicationEvent::OutlookCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::RemoteRecovered,
        })
        .unwrap();
    let recovered = rendered_application_rows(&application).join("\n");
    assert!(recovered.contains("unfinished thought"));
    assert!(recovered.contains("Outlook studio"));
    assert!(!recovered.contains("Reconnecting to Suru…"));
}

#[test]
fn a_revoked_remote_returns_to_the_local_landing_with_the_session_composer_recovered() {
    let mut application = application_looking_at_studio();
    let session_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(session_id, std::path::Path::new("."), 1),
        ))
        .unwrap();
    type_terminal_text(&mut application, "words worth keeping");
    assert!(matches!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::AdmitPrompt { .. }
    ));

    assert_eq!(
        application
            .handle_event(ApplicationEvent::OutlookCatalog {
                outlook: Outlook::Remote("studio".to_owned()),
                event: ManagedEvent::RemoteFailed {
                    status: RemoteStatus::Revoked,
                    message: "Remote revoked this Pairing".to_owned(),
                },
            })
            .unwrap(),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            catalog_outlooks: HashSet::new(),
        }
    );

    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("words worth keeping"));
    assert!(landing.contains("Remote revoked this Pairing"));
    assert!(!landing.contains("Outlook studio"));
}

#[test]
fn choosing_local_again_restores_the_local_outlook_and_workspace() {
    let local_workspace = std::env::current_dir().expect("read local Workspace");
    let mut application = Application::new(&local_workspace, Default::default());
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);

    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Up);

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            catalog_outlooks: HashSet::new(),
        }
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Outlook studio")
    );

    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .unwrap()
    else {
        panic!("the local picker asks the local Server for its Sessions");
    };
    assert_eq!(request.outlook(), &Outlook::Local);
    assert_eq!(
        request.scope(),
        &suru::tui::SessionListScope::CurrentWorkspace(local_workspace)
    );
}

#[test]
fn returning_to_an_outlook_restores_its_stable_selection_presentation() {
    let selection = AgentSelection {
        provider: ProviderId::new("generic-provider"),
        model: ModelId::new("native-model-id"),
        options: Vec::new(),
    };
    let mut application = Application::default();
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424)
                .with_landing_agent_selection(Some(selection)),
        )))
        .expect("restore the local Landing selection");
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("open the local Model picker")
    else {
        panic!("opening the Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("generic-provider"),
                    display_name: "Generic Provider".to_owned(),
                    models: vec![model_descriptor(
                        "generic-provider",
                        "native-model-id",
                        "Friendly Model",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("load the local Model catalog");
    press(&mut application, KeyCode::Esc);

    turn_to_studio(&mut application);
    open_connect(&mut application);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![studio_remote()]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Up);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            catalog_outlooks: HashSet::new(),
        }
    );

    let local_again = rendered_application_rows_at(&application, 100, 16).join("\n");
    assert!(local_again.contains("generic-provider · native-model-id"));
    assert!(!local_again.contains("Friendly Model"));
}

#[test]
fn choosing_the_current_outlook_only_closes_the_picker() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "selecting Local while already local must not detach the current Session stream"
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Paired Remotes")
    );
}

#[test]
fn equal_session_ids_in_different_outlooks_keep_separate_drafts() {
    let session_id = SessionId::new();
    let mut application = Application::default();
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(session_id, std::path::Path::new("."), 1),
        ))
        .unwrap();
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "local-only draft".to_owned(),
        )))
        .unwrap();

    turn_to_studio(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(session_id, std::path::Path::new("."), 1),
        ))
        .unwrap();
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("local-only draft")
    );

    open_connect(&mut application);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![studio_remote()]))
        .unwrap();
    press(&mut application, KeyCode::Up);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            catalog_outlooks: HashSet::new(),
        }
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(session_id, std::path::Path::new("."), 1),
        ))
        .unwrap();
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("local-only draft")
    );
}

#[test]
fn a_remote_catalog_change_refreshes_the_remote_sidebar() {
    let mut application = Application::default();
    turn_to_studio(&mut application);
    let ApplicationTransition::ListSessions(initial) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SidebarToggle,
        )))
        .unwrap()
    else {
        panic!("revealing the Remote Sidebar asks for its Sessions");
    };
    assert_eq!(initial.outlook(), &Outlook::Remote("studio".to_owned()));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request: initial,
            sessions: Vec::new(),
        })
        .unwrap();

    let ApplicationTransition::ListSessions(refresh) = application
        .handle_event(ApplicationEvent::OutlookCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::SessionCreated(SessionCreated {
                session_id: SessionId::new(),
            }),
        })
        .unwrap()
    else {
        panic!("a Remote catalog change refreshes its visible Sidebar");
    };
    assert_eq!(refresh.outlook(), &Outlook::Remote("studio".to_owned()));
}

#[test]
fn closing_the_workspace_picker_cancels_its_pending_resolution() {
    let mut application = Application::default();
    turn_to_studio(&mut application);
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorkspaceList,
        )))
        .unwrap()
    else {
        panic!("opening the Workspace picker asks for Sessions");
    };
    let remote_only = std::path::PathBuf::from("remote-only-workspace");
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![SessionListItem::Readable(Box::new(SessionSummary {
                session: Session {
                    id: SessionId::new(),
                    workspace: Workspace {
                        path: remote_only.clone(),
                    },
                    agent_selection: None,
                    agent_selection_availability: ModelAvailability::Available,
                    status: SessionStatus::Idle,
                    working_since: None,
                    parent: None,
                },
                title: "Remote work".to_owned(),
                emoji: None,
                settled_at: None,
                total_usage: None,
                created_at: SessionTimestamp(1),
                updated_at: SessionTimestamp(2),
            }))],
        })
        .unwrap();
    press(&mut application, KeyCode::Down);
    let ApplicationTransition::ResolveWorkspace {
        outlook,
        surface,
        request_id,
        ..
    } = press(&mut application, KeyCode::Enter)
    else {
        panic!("choosing a Workspace asks its Server to resolve it");
    };
    assert_eq!(
        press(&mut application, KeyCode::Esc),
        ApplicationTransition::CancelWorkspaceResolution(
            WorkspaceResolutionSurface::WorkspacePicker,
        )
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::WorkspaceResolved {
                outlook,
                surface,
                request_id,
                result: Ok(Workspace {
                    path: remote_only.clone(),
                }),
            })
            .unwrap(),
        ApplicationTransition::Continue
    );

    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .unwrap()
    else {
        panic!("opening the Session picker asks for Sessions");
    };
    assert_ne!(
        request.scope(),
        &suru::tui::SessionListScope::CurrentWorkspace(remote_only)
    );
}

#[test]
fn a_remote_session_row_carries_its_origin_into_attachment() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);

    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .unwrap()
    else {
        panic!("opening the picker asks the Remote for its Sessions");
    };
    assert_eq!(request.outlook(), &Outlook::Remote("studio".to_owned()));
    let session_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![SessionListItem::Readable(Box::new(SessionSummary {
                session: Session {
                    id: session_id,
                    workspace: Workspace { path: ".".into() },
                    agent_selection: None,
                    agent_selection_availability: ModelAvailability::Available,
                    status: SessionStatus::Idle,
                    working_since: None,
                    parent: None,
                },
                title: "Remote work".to_owned(),
                emoji: None,
                settled_at: None,
                total_usage: None,
                created_at: SessionTimestamp(1),
                updated_at: SessionTimestamp(2),
            }))],
        })
        .unwrap();

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(SessionReference {
            origin: Outlook::Remote("studio".to_owned()),
            session_id,
        })
    );
}

#[test]
fn a_remote_workspace_pick_is_validated_by_that_remote() {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![Remote {
            name: "studio".to_owned(),
            fingerprint: "studio-fingerprint".to_owned(),
            addresses: vec!["10.0.0.8:7777".parse().unwrap()],
            status: RemoteStatus::Available,
        }]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Enter);

    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorkspaceList,
        )))
        .unwrap()
    else {
        panic!("opening the Workspace picker asks the Remote for its Sessions");
    };
    let remote_only = std::env::current_dir()
        .expect("read fixture root")
        .join("path-that-exists-only-on-the-remote");
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![SessionListItem::Readable(Box::new(SessionSummary {
                session: Session {
                    id: SessionId::new(),
                    workspace: Workspace {
                        path: remote_only.clone(),
                    },
                    agent_selection: None,
                    agent_selection_availability: ModelAvailability::Available,
                    status: SessionStatus::Idle,
                    working_since: None,
                    parent: None,
                },
                title: "Remote work".to_owned(),
                emoji: None,
                settled_at: None,
                total_usage: None,
                created_at: SessionTimestamp(1),
                updated_at: SessionTimestamp(2),
            }))],
        })
        .unwrap();
    press(&mut application, KeyCode::Down);

    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::ResolveWorkspace {
            outlook: Outlook::Remote("studio".to_owned()),
            surface: WorkspaceResolutionSurface::WorkspacePicker,
            request_id: 2,
            request: suru::protocol::ResolveWorkspaceRequest {
                base: None,
                path: remote_only,
            },
        }
    );
}

fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    press_with(application, code, KeyModifiers::NONE)
}

fn application_looking_at_studio() -> Application {
    let mut application = Application::default();
    type_terminal_text(&mut application, "/connect");
    press(&mut application, KeyCode::Enter);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![studio_remote()]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(&mut application, KeyCode::Down);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote("studio".to_owned()),
            catalog_outlooks: HashSet::from([Outlook::Remote("studio".to_owned())]),
        }
    );
    application
}

fn studio_remote() -> Remote {
    Remote {
        name: "studio".to_owned(),
        fingerprint: "studio-fingerprint".to_owned(),
        addresses: vec!["10.0.0.8:7777".parse().unwrap()],
        status: RemoteStatus::Available,
    }
}

fn open_connect(application: &mut Application) {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ConnectOpen,
        )))
        .expect("open the Connect picker");
}

fn turn_to_studio(application: &mut Application) {
    open_connect(application);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![studio_remote()]))
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    press(application, KeyCode::Down);
    assert_eq!(
        press(application, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Remote("studio".to_owned()),
            catalog_outlooks: HashSet::from([Outlook::Remote("studio".to_owned())]),
        }
    );
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
        status: RemoteStatus::Available,
    };
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            remote("studio"),
            remote("old"),
            remote("revoked"),
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
                protocol_version: Some(28),
                status: RemoteStatus::Available,
            }),
        })
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "old".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(27),
                status: RemoteStatus::ProtocolMismatch,
            }),
        })
        .unwrap();
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "revoked".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: None,
                status: RemoteStatus::Revoked,
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
    assert!(picker.contains("revoked  Revoked"));
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
            status: RemoteStatus::Available,
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
            status: RemoteStatus::Available,
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
            status: RemoteStatus::Available,
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
            status: RemoteStatus::Available,
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

#[test]
fn the_address_cursor_is_drawn_before_the_keys_reach_the_address_list() {
    let application = configure_remote_draft();

    let draft = rendered_application_rows(&application).join("\n");
    assert!(
        draft.contains("› 1. 10.0.0.8:7777"),
        "the row Shift+↑↓ would reorder is marked while the name field has the keys"
    );
    assert!(draft.contains("  2. 192.168.1.24:7777"));
    assert_eq!(
        connect_text_on(&application, Color::DarkGray).trim(),
        "› 1. 10.0.0.8:7777",
        "the address cursor is dimmed while the keys are in the name field"
    );
    assert_eq!(
        connect_text_on(&application, Color::Blue).trim(),
        "> studio",
        "the name field is lit because it is where the keys are"
    );
}

#[test]
fn arrows_carry_the_keys_from_the_name_field_into_the_address_list() {
    let mut application = configure_remote_draft();

    press(&mut application, KeyCode::Down);
    assert_eq!(
        connect_text_on(&application, Color::Blue).trim(),
        "› 1. 10.0.0.8:7777",
        "one Down press lights the address the cursor was already marking"
    );

    press_with(&mut application, KeyCode::Down, KeyModifiers::SHIFT);
    let reordered = rendered_application_rows(&application).join("\n");
    assert!(reordered.contains("  1. 192.168.1.24:7777"));
    assert!(reordered.contains("› 2. 10.0.0.8:7777"));
}

#[test]
fn arrows_wrap_between_the_address_list_and_the_name_field() {
    let mut application = configure_remote_draft();

    press(&mut application, KeyCode::Up);
    assert_eq!(
        connect_text_on(&application, Color::Blue).trim(),
        "› 2. 192.168.1.24:7777",
        "Up from the name field enters the address list at its last row"
    );

    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Char('!'));
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("> studio!"),
        "Down past the last address hands the keys back to the name field"
    );

    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Up);
    press(&mut application, KeyCode::Char('?'));
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("> studio!?"),
        "Up from the first address hands the keys back to the name field"
    );
}

#[test]
fn the_priority_order_is_reorderable_before_the_keys_reach_the_address_list() {
    let mut application = configure_remote_draft();

    press_with(&mut application, KeyCode::Down, KeyModifiers::SHIFT);
    let reordered = rendered_application_rows(&application).join("\n");
    assert!(reordered.contains("  1. 192.168.1.24:7777"));
    assert!(
        reordered.contains("› 2. 10.0.0.8:7777"),
        "Shift+↓ moves the marked address without the reader entering the list first"
    );
    assert_eq!(
        connect_text_on(&application, Color::Blue).trim(),
        "> studio",
        "reordering leaves the keys in the name field, so the name stays typeable"
    );

    press(&mut application, KeyCode::Char('!'));
    press_with(&mut application, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::RedeemInvite(RedeemInviteRequest {
            invite: "suru-v1-example".to_owned(),
            name: Some("studio!".to_owned()),
            addresses: vec![
                "10.0.0.8:7777".parse().unwrap(),
                "192.168.1.24:7777".parse().unwrap()
            ],
        })
    );
}

#[test]
fn the_configure_remote_screen_teaches_the_keys_it_answers() {
    let hint = rendered_application_rows(&configure_remote_draft()).join("\n");

    assert!(hint.contains("↑↓ move · Shift+↑↓ reorder · Tab field · Enter pair · Esc cancel"));
}

/// A Connect draft holding the Invite's two addresses, with the keys where the
/// overlay leaves them: in the Remote name field.
fn configure_remote_draft() -> Application {
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
                addresses: vec![
                    "10.0.0.8:7777".parse().unwrap(),
                    "192.168.1.24:7777".parse().unwrap(),
                ],
            },
        })
        .unwrap();
    press(&mut application, KeyCode::Enter);
    application
}

/// The Connect overlay text this frame draws on `background`, read across the
/// whole terminal because the overlay is centred in it.
fn connect_text_on(application: &Application, background: Color) -> String {
    text_on(application, background, (80, 24), 0..80)
}
