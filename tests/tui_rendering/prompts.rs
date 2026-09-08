//! Prompt submission: steering, queueing, admission, and failure recovery.

use crate::support::{
    FailedTurnFixture, enter_active_session, enter_session, rendered_application_rows,
    type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Outlook, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, SessionChange,
        SessionId, SessionReference, SessionRevision, SessionUpdate,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, command_for_terminal_event,
    },
};

#[test]
fn new_session_keybinding_defers_creation_until_the_next_prompt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (_, old_snapshot, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "discard this draft".to_owned(),
        )))
        .expect("type a Session draft");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
            )))
            .expect("begin semantic leader keybinding"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            )))
            .expect("invoke new Session"),
        ApplicationTransition::DetachSession
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            old_snapshot.clone(),
        )))
        .expect("ignore a queued event from the detached Session");
    let landing = rendered_application_rows(&application).join("\n");
    assert!(!landing.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
    assert!(landing.contains("Type a prompt"));
    assert!(!landing.contains("discard this draft"));
    assert!(!landing.contains("Long-running work"));

    application
        .handle_event(ApplicationEvent::SessionAttached(old_snapshot))
        .expect("reattach the independently addressable active Session");
    let reattached = rendered_application_rows(&application).join("\n");
    assert!(reattached.contains("Long-running work"));
    assert!(reattached.contains("Working ("));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionNew,
            )))
            .expect("return to landing through the same semantic command"),
        ApplicationTransition::DetachSession
    );

    type_terminal_text(&mut application, "Next Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit the next landing Prompt")
    else {
        panic!("the first Prompt after /new should create a Session");
    };
    assert_eq!(request.prompt.text, "Next Prompt");
    assert_eq!(request.workspace.path, workspace.path());
}

#[test]
fn new_session_releases_a_detached_prompt_after_its_admission_succeeds() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Continue in the old Session".to_owned(),
        )))
        .expect("type an in-flight Prompt");
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("begin Prompt admission")
    else {
        panic!("the old Session Prompt should be admitted");
    };
    let admitted_prompt_id = request.prompt.id;

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionNew,
            )))
            .expect("detach while admission remains in flight"),
        ApplicationTransition::DetachSession
    );
    application
        .handle_event(ApplicationEvent::PromptAdmissionSucceeded {
            session,
            prompt_id: admitted_prompt_id,
        })
        .expect("acknowledge the detached Prompt admission");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Start separate work".to_owned(),
        )))
        .expect("type the next landing Prompt");

    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit after detached admission settles")
    else {
        panic!("a settled detached admission must not block landing submission");
    };
    assert_eq!(request.prompt.text, "Start separate work");
}

#[test]
fn provisional_steer_is_immediate_single_and_reconciles_in_place() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, initial_snapshot) = enter_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Use the smaller interface".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt {
        session: admitted_to,
        request,
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    assert_eq!(admitted_to.origin, suru::protocol::Outlook::Local);
    assert_eq!(admitted_to.session_id, session_id);
    let prompt_id = request.prompt.id;
    let provisional = rendered_application_rows(&application).join("\n");
    assert_eq!(provisional.matches("Use the smaller interface").count(), 1);
    assert!(provisional.contains("Type a prompt"));

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("ignore overlapping submit key event"),
        ApplicationTransition::Continue
    );
    application
        .handle_event(ApplicationEvent::PromptAdmissionSucceeded {
            session: admitted_to,
            prompt_id,
        })
        .expect("handle Prompt admission acknowledgement");
    assert_eq!(
        rendered_application_rows(&application)
            .join("\n")
            .matches("Use the smaller interface")
            .count(),
        1
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            delivered_update(
                session_id,
                SessionRevision(initial_snapshot.revision.0 + 1),
                prompt_id,
                &request.prompt.text,
            ),
        )))
        .expect("apply authoritative Prompt delivery");
    let reconciled = rendered_application_rows(&application).join("\n");
    assert_eq!(reconciled.matches("Use the smaller interface").count(), 1);
}

#[test]
fn admitted_active_steer_stays_visible_while_the_composer_accepts_another_prompt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, snapshot, _) = enter_active_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Keep this pending steer visible".to_owned(),
        )))
        .expect("type active steer");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit active steer")
    else {
        panic!("active steer should request Prompt admission");
    };
    let prompt_id = request.prompt.id;
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: request.prompt.text,
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(3),
                        status: PromptStatus::Pending,
                        skill_invocations: Vec::new(),
                    },
                }],
            },
        )))
        .expect("reconcile pending active steer");
    let pending = rendered_application_rows(&application).join("\n");
    assert_eq!(
        pending.matches("Keep this pending steer visible").count(),
        1
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "A second steer".to_owned(),
        )))
        .expect("type another steer while the first is pending");
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit another steer"),
        ApplicationTransition::AdmitPrompt { .. }
    ));
}

#[test]
fn queued_prompt_docks_immediately_and_scoped_mode_preserves_the_draft() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, snapshot, _) = enter_active_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Run this later".to_owned(),
        )))
        .expect("type queued Prompt");
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        ))),
        Some(CommandId::SubmitQueue)
    );
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitQueue))
        .expect("submit queued Prompt")
    else {
        panic!("Alt+Enter should request queued Prompt admission");
    };
    assert_eq!(request.delivery, PromptDelivery::Queue);
    let prompt_id = request.prompt.id;
    let optimistic = rendered_application_rows(&application).join("\n");
    assert!(optimistic.contains("Pending"));
    assert_eq!(optimistic.matches("Run this later").count(), 1);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: request.prompt.text,
                        delivery: PromptDelivery::Queue,
                        admission_order: PromptOrder(3),
                        status: PromptStatus::Pending,
                        skill_invocations: Vec::new(),
                    },
                }],
            },
        )))
        .expect("apply authoritative queued Prompt");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "keep this draft".to_owned(),
        )))
        .expect("type a competing draft");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL,
            )))
            .expect("start command leader"),
        ApplicationTransition::Continue
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("open queued-Prompt mode");
    let ApplicationTransition::PromotePrompt {
        session: promoted_in,
        prompt_id: promoted,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("promote selected queued Prompt")
    else {
        panic!("Enter in queued-Prompt mode should promote the selection");
    };
    assert_eq!((promoted_in.session_id, promoted), (session_id, prompt_id));
    application
        .handle_event(ApplicationEvent::SessionOperationFailed(
            "competing mutation lost".to_owned(),
        ))
        .expect("report failed mutation");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("keep this draft")
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("restart command leader");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("reopen queued-Prompt mode");
    let ApplicationTransition::CancelPrompt {
        session: cancelled_in,
        prompt_id: cancelled,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )))
        .expect("cancel selected queued Prompt")
    else {
        panic!("Ctrl+D in queued-Prompt mode should cancel the selection");
    };
    assert_eq!(
        (cancelled_in.session_id, cancelled),
        (session_id, prompt_id)
    );
}

#[test]
fn escape_confirmation_is_local_and_targets_the_observed_active_turn() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (expected_session_id, snapshot, _active_turn_id) =
        enter_active_session(&mut application, workspace.path());
    let mut observer = Application::new(workspace.path(), Default::default());
    observer
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a second local observer");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request interruption"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Esc again")
    );
    assert!(
        !rendered_application_rows(&observer)
            .join("\n")
            .contains("Esc again"),
        "interruption confirmation must remain client-local"
    );

    let ApplicationTransition::InterruptSession {
        session: session_id,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("confirm interruption")
    else {
        panic!("the second Esc should issue the Session's interruption");
    };
    assert_eq!(session_id.origin, suru::protocol::Outlook::Local);
    assert_eq!(session_id.session_id, expected_session_id);
}

#[test]
fn escape_confirmation_expires_after_five_seconds() {
    let workspace = workspace_dir();
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let mut application = Application::new(workspace.path(), Default::default())
        .with_presentation_clock(move || {
            *clock.lock().expect("presentation clock remains readable")
        });
    enter_active_session(&mut application, workspace.path());

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("arm interruption"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Esc again to interrupt")
    );

    let start = *now.lock().expect("presentation clock remains writable");
    *now.lock().expect("presentation clock remains writable") = start + Duration::from_secs(5);
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("arm interruption again"),
        ApplicationTransition::Continue,
        "an expired first press cannot become a delayed interruption, even without a presentation tick"
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Esc again to interrupt"),
        "the expired press starts a fresh confirmation window"
    );
}

#[test]
fn failed_admission_restores_stable_prompt_and_saves_intervening_input_to_history() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    enter_session(&mut application, workspace.path());

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Do not lose this Prompt".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    let prompt_id = request.prompt.id;
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "newer local text".to_owned(),
        )))
        .expect("type while admission is pending");
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session: session.clone(),
            prompt_id,
            error: "server unavailable".to_owned(),
        })
        .expect("roll back failed admission");
    let restored = rendered_application_rows(&application).join("\n");
    assert_eq!(restored.matches("Do not lose this Prompt").count(), 1);
    assert!(!restored.contains("newer local text"));
    assert!(!restored.contains("Use the smaller interface"));

    let ApplicationTransition::AdmitPrompt {
        request: exact_retry,
        ..
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("retry restored Prompt")
    else {
        panic!("restored Prompt should be retryable");
    };
    assert_eq!(exact_retry.prompt.id, prompt_id);
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session,
            prompt_id,
            error: "still unavailable".to_owned(),
        })
        .expect("restore the exact retry");

    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("navigate to displaced local input");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("newer local text")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryNext))
        .expect("return to restored failed Prompt");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Do not lose this Prompt")
    );
}

#[test]
fn an_admission_result_from_another_origin_cannot_settle_the_pending_prompt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    enter_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "Keep this origin");
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the local Prompt")
    else {
        panic!("a Session Prompt should request admission");
    };
    let prompt_id = request.prompt.id;
    let other_origin =
        SessionReference::new(Outlook::Remote("studio".to_owned()), session.session_id);

    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session: other_origin,
            prompt_id,
            error: "wrong Server".to_owned(),
        })
        .expect("ignore an answer from another origin");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("the other origin leaves admission pending"),
        ApplicationTransition::Continue
    );
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("wrong Server")
    );

    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session,
            prompt_id,
            error: "local Server unavailable".to_owned(),
        })
        .expect("accept the answer from the owning origin");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("local Server unavailable")
    );
}

#[test]
fn authoritative_delivery_after_an_ambiguous_failure_removes_the_restored_retry() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, snapshot) = enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Accepted despite transport failure".to_owned(),
        )))
        .expect("type steer");
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit steer")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session,
            prompt_id: request.prompt.id,
            error: "response connection closed".to_owned(),
        })
        .expect("restore ambiguously failed Prompt");
    assert_eq!(
        rendered_application_rows(&application)
            .join("\n")
            .matches("Accepted despite transport failure")
            .count(),
        1
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("select the restored retry");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            delivered_update(
                session_id,
                SessionRevision(snapshot.revision.0 + 1),
                request.prompt.id,
                &request.prompt.text,
            ),
        )))
        .expect("reconcile late authoritative delivery");
    let reconciled = rendered_application_rows(&application).join("\n");
    assert_eq!(
        reconciled
            .matches("Accepted despite transport failure")
            .count(),
        1
    );
    assert!(reconciled.contains("Type a prompt"));
    assert!(!reconciled.contains("response connection closed"));
    type_terminal_text(&mut application, "next draft");
    let buffer = crate::support::rendered_application_buffer(&application, 100, 32);
    let cursor = crate::support::rendered_application_cursor_at(&application, 100, 32);
    for x in cursor.x - 10..cursor.x {
        assert!(
            !buffer[(x, cursor.y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }
}

fn delivered_update(
    session_id: SessionId,
    revision: SessionRevision,
    prompt_id: PromptId,
    text: &str,
) -> SessionUpdate {
    let delivered = FailedTurnFixture::new(prompt_id, text, PromptOrder(revision.0));
    SessionUpdate {
        session_id,
        revision,
        changes: delivered.into_changes(),
    }
}

#[test]
fn failed_admission_drops_the_intervening_drafts_selection_before_restoring_text() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    enter_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "old draft");
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap()
    else {
        panic!("submit the original draft");
    };
    type_terminal_text(&mut application, "new draft");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session,
            prompt_id: request.prompt.id,
            error: "server unavailable".to_owned(),
        })
        .unwrap();
    let buffer = crate::support::rendered_application_buffer(&application, 100, 32);
    let cursor = crate::support::rendered_application_cursor_at(&application, 100, 32);
    for x in cursor.x - 9..cursor.x {
        assert!(
            !buffer[(x, cursor.y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }
    type_terminal_text(&mut application, " appended");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("old draft appended")
    );
}
