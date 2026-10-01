//! Prompt submission: steering, queueing, admission, and failure recovery —
//! including a Prompt held behind a requested Compaction, which accepts no
//! steer (ADR 0041).

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
        Activity, ActivityId, ActivityStatus, CompactionTrigger, InitialPrompt, Message, MessageId,
        MessageRole, MessageStatus, Outlook, Prompt, PromptDelivery, PromptId, PromptOrder,
        PromptStatus, SessionChange, SessionId, SessionReference, SessionRevision, SessionStatus,
        SessionTimestamp, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus,
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
    assert_eq!(request.execution_directory.path, workspace.path());
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
                        attachments: Vec::new(),
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
                        attachments: Vec::new(),
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

#[test]
fn unavailable_worktree_admission_preserves_actionable_error_and_exact_prompt_retry() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Continue retained work".to_owned(),
        )))
        .unwrap();
    let ApplicationTransition::AdmitPrompt { session, request } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .unwrap()
    else {
        panic!("admit");
    };
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session: session.clone(),
            prompt_id: request.prompt.id,
            error: "Worktree unavailable: retained branch is occupied; resolve it and retry"
                .to_owned(),
        })
        .unwrap();
    let rows = rendered_application_rows(&application).join("\n");
    assert!(rows.contains("Continue retained work"));
    assert!(rows.contains("Worktree unavailable"), "{rows}");
    let ApplicationTransition::AdmitPrompt {
        session: destination,
        request: retry,
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .unwrap()
    else {
        panic!("retry");
    };
    assert_eq!(destination, session);
    assert_eq!(retry, request);
}

/// A Session running the Turn a Compaction request began (ADR 0041), open in
/// `application`: Working, its Compaction under way, and the Turn taking no
/// steer.
fn enter_compacting_session(application: &mut Application) -> CompactingSession {
    let workspace = workspace_dir();
    let (_, mut snapshot) = enter_session(application, workspace.path());
    let started_at = SessionTimestamp::now();
    let mut turn = Turn::requested_compaction(None);
    turn.started_at = Some(started_at);
    let turn_id = turn.id;
    let compaction_id = ActivityId::new();
    snapshot.turns.push(turn);
    snapshot.activities.push(Activity::Compaction {
        id: compaction_id,
        turn_id,
        status: ActivityStatus::Active,
        trigger: CompactionTrigger::Manual,
        before_tokens: None,
        after_tokens: None,
        error: None,
        summary: None,
        summary_truncated: false,
    });
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: compaction_id,
    });
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(started_at);
    snapshot.revision = SessionRevision(snapshot.revision.0 + 1);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("open the compacting Session");
    CompactingSession {
        _workspace: workspace,
        snapshot,
        turn_id,
        compaction_id,
    }
}

struct CompactingSession {
    _workspace: crate::support::WorkspaceDir,
    snapshot: suru::protocol::SessionSnapshot,
    turn_id: TurnId,
    compaction_id: ActivityId,
}

impl CompactingSession {
    fn session_id(&self) -> SessionId {
        self.snapshot.session.id
    }

    /// The next revision of the Session, carrying `changes`.
    fn update(&mut self, changes: Vec<SessionChange>) -> SessionEvent {
        self.snapshot.revision = SessionRevision(self.snapshot.revision.0 + 1);
        SessionEvent::Updated(SessionUpdate {
            session_id: self.session_id(),
            revision: self.snapshot.revision,
            changes,
        })
    }

    /// The Session admitting `prompt` as a steer while its Compaction runs,
    /// which holds it, owed the next Turn.
    fn held(&mut self, prompt: &InitialPrompt) -> SessionEvent {
        self.update(vec![SessionChange::PromptAdded {
            prompt: Prompt {
                id: prompt.id,
                text: prompt.text.clone(),
                delivery: PromptDelivery::Steer,
                admission_order: PromptOrder(3),
                status: PromptStatus::Pending,
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        }])
    }

    /// The Compaction's Turn settling as `status` short of completing, and
    /// the Prompt held behind it withdrawn in the same revision, as the
    /// Session commits them.
    fn ended_short(&mut self, prompt: PromptId, status: TurnStatus) -> SessionEvent {
        let (compaction, error) = match status {
            TurnStatus::Interrupted => (ActivityStatus::Interrupted, None),
            _ => (
                ActivityStatus::Failed,
                Some("Conversation too long to summarise".to_owned()),
            ),
        };
        let (turn_id, compaction_id) = (self.turn_id, self.compaction_id);
        self.update(vec![
            SessionChange::PromptStatusChanged {
                prompt_id: prompt,
                status: PromptStatus::Cancelled,
            },
            SessionChange::CompactionSettled {
                activity_id: compaction_id,
                status: compaction,
                before_tokens: None,
                after_tokens: None,
                error,
                summary: None,
                summary_truncated: false,
            },
            SessionChange::TurnStatusChanged {
                turn_id,
                status,
                settled_at: Some(SessionTimestamp::now()),
            },
            SessionChange::SessionWorkingChanged {
                working_since: None,
            },
            SessionChange::SessionStatusChanged {
                status: SessionStatus::Idle,
            },
        ])
    }

    /// The Compaction completing and the Prompt held behind it beginning the
    /// next Turn.
    fn completed(&mut self, prompt: &InitialPrompt) -> SessionEvent {
        let (turn_id, compaction_id) = (self.turn_id, self.compaction_id);
        let next = Turn {
            id: TurnId::new(),
            prompt_id: Some(prompt.id),
            started_at: Some(SessionTimestamp::now()),
            ..Turn::unprompted(None)
        };
        let message = Message {
            id: MessageId::new(),
            turn_id: next.id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: prompt.text.clone(),
            truncated: false,
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        };
        self.update(vec![
            SessionChange::CompactionSettled {
                activity_id: compaction_id,
                status: ActivityStatus::Completed,
                before_tokens: Some(182_000),
                after_tokens: Some(31_000),
                error: None,
                summary: None,
                summary_truncated: false,
            },
            SessionChange::TurnStatusChanged {
                turn_id,
                status: TurnStatus::Completed,
                settled_at: Some(SessionTimestamp::now()),
            },
            SessionChange::PromptStatusChanged {
                prompt_id: prompt.id,
                status: PromptStatus::Delivered,
            },
            SessionChange::TurnAdded { turn: next },
            SessionChange::MessageAdded { message },
        ])
    }
}

/// Writes `text` into the open Session's composer and sends it as a steer,
/// answering the Prompt sent.
fn send_steer(application: &mut Application, text: &str) -> InitialPrompt {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            text.to_owned(),
        )))
        .expect("write a Prompt");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("send it")
    else {
        panic!("a Prompt written in a Session asks for its admission");
    };
    request.prompt
}

/// What the composer holds, read by sending it.
fn composer_holds(application: &mut Application) -> Option<InitialPrompt> {
    match application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("send whatever the composer holds")
    {
        ApplicationTransition::AdmitPrompt { request, .. } => Some(request.prompt),
        _ => None,
    }
}

fn drawn(application: &Application, text: &str) -> usize {
    rendered_application_rows(application)
        .join("\n")
        .matches(text)
        .count()
}

#[test]
fn a_prompt_held_behind_a_failed_compaction_comes_back_to_the_composer_it_was_written_in() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let mut session = enter_compacting_session(&mut application);
    let mut observer = Application::new(workspace_dir().path(), Default::default());
    observer
        .handle_event(ApplicationEvent::SessionAttached(session.snapshot.clone()))
        .expect("a second client watches the Session");

    let prompt = send_steer(&mut application, "Now the lexer");
    let held = session.held(&prompt);
    for client in [&mut application, &mut observer] {
        client
            .handle_event(ApplicationEvent::Session(held.clone()))
            .expect("the Session holds the Prompt");
        assert_eq!(
            drawn(client, "Now the lexer"),
            1,
            "every client draws the held Prompt as the Message it will become"
        );
    }

    let ended = session.ended_short(prompt.id, TurnStatus::Failed);
    for client in [&mut application, &mut observer] {
        client
            .handle_event(ApplicationEvent::Session(ended.clone()))
            .expect("the Compaction fails and the Prompt is withdrawn");
    }
    let returned =
        composer_holds(&mut application).expect("the withdrawn Prompt comes back to its writer");
    assert_eq!(returned.text, "Now the lexer");
    assert_eq!(
        composer_holds(&mut observer),
        None,
        "a client that did not write it is handed nothing"
    );
}

#[test]
fn a_prompt_held_behind_an_interrupted_compaction_comes_back_to_the_composer_too() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let mut session = enter_compacting_session(&mut application);
    let prompt = send_steer(&mut application, "Now the lexer");
    application
        .handle_event(ApplicationEvent::Session(session.held(&prompt)))
        .expect("the Session holds the Prompt");

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("interrupt the Compaction");
    }
    assert_eq!(
        drawn(&application, "Now the lexer"),
        1,
        "the Prompt stays held while the Compaction is stopped"
    );
    application
        .handle_event(ApplicationEvent::Session(
            session.ended_short(prompt.id, TurnStatus::Interrupted),
        ))
        .expect("the Compaction stops and the Prompt is withdrawn");
    let returned = composer_holds(&mut application).expect("the withdrawn Prompt comes back");
    assert_eq!(returned.text, "Now the lexer");
    assert_ne!(
        returned.id, prompt.id,
        "sent again, it is a new Prompt: the one it was stays withdrawn"
    );
}

#[test]
fn a_prompt_held_behind_a_compaction_that_completes_begins_the_next_turn_and_stays_sent() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let mut session = enter_compacting_session(&mut application);
    let prompt = send_steer(&mut application, "Now the lexer");
    application
        .handle_event(ApplicationEvent::Session(session.held(&prompt)))
        .expect("the Session holds the Prompt");

    application
        .handle_event(ApplicationEvent::Session(session.completed(&prompt)))
        .expect("the Compaction completes and the Prompt begins the next Turn");
    assert_eq!(drawn(&application, "Now the lexer"), 1);
    assert_eq!(
        composer_holds(&mut application),
        None,
        "a Prompt that was delivered is not handed back"
    );
}
