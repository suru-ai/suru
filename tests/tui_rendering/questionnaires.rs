use crate::support::{
    connected_application, enter_active_session, rendered_application_rows, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityId, Answer, Question, QuestionAnswer, QuestionChoice, Questionnaire,
        QuestionnaireId, QuestionnaireOutcome, QuestionnaireSubmission, TranscriptItem,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

fn invoke(app: &mut Application, command: SemanticCommandId) -> ApplicationTransition {
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        command,
    )))
    .unwrap()
}
fn key(app: &mut Application, code: KeyCode) -> ApplicationTransition {
    app.handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap()
}

#[test]
fn questionnaire_panel_requires_explicit_answer_review_and_submit_and_preserves_both_drafts() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "My composer draft");
    let questionnaire = Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "q".into(),
            title: None,
            text: "Choose the execution target".into(),
            choices: vec![QuestionChoice {
                id: "local".into(),
                label: "Local".into(),
                description: None,
                recommended: true,
            }],
            multiple: false,
            freeform: true,
            secret: false,
            required: true,
        }],
    };
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id,
        questionnaire: questionnaire.clone(),
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    type_terminal_text(&mut app, " stays");
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("My composer draft stays"),
        "arrival keeps composer focus: {screen}"
    );
    assert!(screen.contains("Questionnaire pending"));
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("[ ] Local (recommended)"),
        "recommendation is not selected: {screen}"
    );
    key(&mut app, KeyCode::Enter);
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("requires a supported answer")
    );
    type_terminal_text(&mut app, "Use the staging machine");
    let edited = rendered_application_rows(&app).join("\n");
    for kind in [
        crossterm::event::KeyEventKind::Release,
        crossterm::event::KeyEventKind::Repeat,
    ] {
        for (code, modifiers) in [
            (KeyCode::Char('x'), KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char(' '), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::CONTROL),
            (KeyCode::Char('d'), KeyModifiers::CONTROL),
            (KeyCode::Esc, KeyModifiers::NONE),
        ] {
            let event = Event::Key(KeyEvent::new_with_kind(code, modifiers, kind));
            assert_eq!(
                app.command_for_terminal_input(event.clone()),
                None,
                "{kind:?} must not dispatch {code:?}"
            );
            assert!(matches!(
                app.handle_terminal_event(event).unwrap(),
                ApplicationTransition::Continue
            ));
        }
    }
    assert_eq!(
        rendered_application_rows(&app).join("\n"),
        edited,
        "release and repeat events leave the edited Answer and panel untouched"
    );

    assert!(matches!(
        key(&mut app, KeyCode::Esc),
        ApplicationTransition::Continue
    ));
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("My composer draft stays")
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    key(&mut app, KeyCode::Enter);
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("Review Answer") && screen.contains("Use the staging machine"),
        "review preserves spaces and hidden draft: {screen}"
    );
    let transition = invoke(&mut app, SemanticCommandId::QuestionnaireSubmit);
    assert!(
        matches!(transition, ApplicationTransition::SubmitQuestionnaire { id, submission: QuestionnaireSubmission::Answer { answer: Answer { questions } }, .. } if id == questionnaire.id && questions == vec![QuestionAnswer::Freeform { text: "Use the staging machine".into() }])
    );
    snapshot.revision.0 += 1;
    let Activity::Questionnaire {
        outcome, answer, ..
    } = snapshot.activities.last_mut().unwrap()
    else {
        unreachable!()
    };
    *outcome = QuestionnaireOutcome::Answered;
    *answer = Some(Answer {
        questions: vec![QuestionAnswer::Freeform {
            text: "Use the staging machine".into(),
        }],
    });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("My composer draft stays")
    );
}

#[test]
fn withdrawn_questionnaire_discards_the_answer_draft_and_explains_the_outcome() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id,
        questionnaire: Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![Question {
                id: "q".into(),
                title: None,
                text: "What should I use?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                secret: false,
                required: true,
            }],
        },
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    type_terminal_text(&mut app, "Obsolete draft");
    let Activity::Questionnaire { outcome, .. } = snapshot.activities.last_mut().unwrap() else {
        unreachable!()
    };
    *outcome = QuestionnaireOutcome::Withdrawn;
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    assert!(matches!(
        invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
        ApplicationTransition::Continue
    ));
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        !screen.contains("Obsolete draft") && screen.contains("Withdrawn"),
        "{screen}"
    );
}

#[test]
fn long_questions_can_be_scrolled_without_losing_the_review_actions() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id,
        turn_id,
        questionnaire: Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![Question {
                id: "q".into(),
                title: None,
                text: format!(
                    "{}Last question detail",
                    "Read this long detail carefully. ".repeat(100)
                ),
                choices: vec![],
                multiple: false,
                freeform: true,
                secret: false,
                required: true,
            }],
        },
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id: id });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("Enter review") && screen.contains("Alt+↑/↓ scroll"),
        "{screen}"
    );
    for _ in 0..100 {
        invoke(&mut app, SemanticCommandId::QuestionnaireScrollDown);
    }
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Last question detail")
    );
}

#[test]
fn selected_answers_have_compact_expandable_history_and_panel_keeps_transcript_navigation() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let id = ActivityId::new();
    let questionnaire = Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "q".into(),
            title: None,
            text: "Which build target?".into(),
            choices: vec![QuestionChoice {
                id: "remote".into(),
                label: "Remote".into(),
                description: None,
                recommended: false,
            }],
            multiple: false,
            freeform: false,
            secret: false,
            required: true,
        }],
    };
    let message_id = suru::protocol::MessageId::new();
    snapshot.messages.push(suru::protocol::Message {
        id: message_id,
        turn_id,
        role: suru::protocol::MessageRole::Agent,
        status: suru::protocol::MessageStatus::Completed,
        content: (0..70).map(|i| format!("Context line {i}\n")).collect(),
        skill_invocations: vec![],
        truncated: false,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Message { message_id });
    snapshot.activities.push(Activity::Questionnaire {
        id,
        turn_id,
        questionnaire,
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id: id });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    type_terminal_text(&mut app, "Keep composer");
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    let before = rendered_application_rows(&app).join("\n");
    let page = Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(
        app.command_for_terminal_input(page.clone()),
        Some(CommandId::ScrollTranscriptPageUp)
    );
    app.handle_terminal_event(page).unwrap();
    let browsed = rendered_application_rows(&app).join("\n");
    assert_ne!(before, browsed);
    assert!(browsed.contains("Which build target?"));
    // Composer editing commands cannot reach the preserved Prompt while the panel owns input.
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Enter);
    let submission = invoke(&mut app, SemanticCommandId::QuestionnaireSubmit);
    let ApplicationTransition::SubmitQuestionnaire {
        submission: QuestionnaireSubmission::Answer { answer },
        ..
    } = submission
    else {
        panic!("explicit selection passes review")
    };
    assert_eq!(
        answer.questions,
        vec![QuestionAnswer::Selected {
            choices: vec!["remote".into()]
        }]
    );
    let Activity::Questionnaire {
        outcome,
        answer: stored,
        ..
    } = snapshot.activities.last_mut().unwrap()
    else {
        unreachable!()
    };
    *outcome = QuestionnaireOutcome::Answered;
    *stored = Some(answer);
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::FollowLatest))
        .unwrap();
    let compact = rendered_application_rows(&app).join("\n");
    assert!(
        compact.contains("Answered") && compact.contains("1 question(s)"),
        "{compact}"
    );
    assert!(!compact.contains("Which build target?"));
    assert!(compact.contains("Keep composer"));
    invoke(&mut app, SemanticCommandId::TranscriptFoldsToggle);
    let expanded = rendered_application_rows(&app).join("\n");
    assert!(
        expanded.contains("Which build target?") && expanded.contains("remote"),
        "{expanded}"
    );
}

fn add_request(
    snapshot: &mut suru::protocol::SessionSnapshot,
    turn_id: suru::protocol::TurnId,
    text: &str,
) -> QuestionnaireId {
    let id = QuestionnaireId::new();
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id,
        questionnaire: Questionnaire {
            id,
            questions: vec![Question {
                id: "question".into(),
                title: None,
                text: text.into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                secret: false,
                required: true,
            }],
        },
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    snapshot.revision.0 += 1;
    id
}

#[test]
fn concurrent_questionnaires_keep_individual_drafts_and_never_take_focus_on_arrival() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "Composer draft");
    let first = add_request(&mut snapshot, turn_id, "First request");
    add_request(&mut snapshot, turn_id, "Second request");
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    type_terminal_text(&mut app, "First answer");
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Questionnaire 1 of 2")
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireRequestNext);
    type_terminal_text(&mut app, "Second answer");
    add_request(&mut snapshot, turn_id, "Third request");
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    type_terminal_text(&mut app, " remains");
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("Questionnaire 2 of 3") && screen.contains("Second answer remains"),
        "{screen}"
    );
    app.handle_terminal_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)))
        .unwrap();
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
        ApplicationTransition::SubmitQuestionnaire { id, submission: QuestionnaireSubmission::Answer { answer }, .. }
        if id == first && answer.questions == vec![QuestionAnswer::Freeform { text: "First answer".into() }])
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireHide);
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Composer draft")
    );
    // Session switching preserves each Answer and the separate composer.
    let mut other_app = connected_application(workspace.path());
    let (_, other, _) = enter_active_session(&mut other_app, workspace.path());
    app.handle_event(ApplicationEvent::SessionAttached(other.clone()))
        .unwrap();
    app.handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    invoke(&mut app, SemanticCommandId::QuestionnaireRequestNext);
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Second answer remains")
    );
    app.handle_event(ApplicationEvent::SessionAttached(other))
        .unwrap();
    type_terminal_text(&mut app, "Other composer");
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Other composer")
    );
}

#[test]
fn pending_transcript_activity_opens_its_own_request() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    add_request(&mut snapshot, turn_id, "First request");
    add_request(&mut snapshot, turn_id, "Second request");
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    let rows = rendered_application_rows(&app);
    let row = rows
        .iter()
        .enumerate()
        .filter(|(_, text)| text.contains("Questionnaire · Pending"))
        .nth(1)
        .unwrap()
        .0;
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rows[row].find("Questionnaire").unwrap() as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    }))
    .unwrap();
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("Second request") && screen.contains("Questionnaire 2 of 2"),
        "{screen}"
    );
}

#[test]
fn catalog_discards_only_unavailable_hidden_drafts_and_ignores_older_availability() {
    use suru::{
        managed_client::ManagedEvent,
        protocol::{SessionRevision, SessionStandingInputs, SessionStandingInputsChanged},
    };
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (session_id, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let first = add_request(&mut snapshot, turn_id, "First request");
    let second = add_request(&mut snapshot, turn_id, "Second request");
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    type_terminal_text(&mut app, "Obsolete answer");
    invoke(&mut app, SemanticCommandId::QuestionnaireRequestNext);
    type_terminal_text(&mut app, "Retained answer");
    let mut other_app = connected_application(workspace.path());
    let (_, other, _) = enter_active_session(&mut other_app, workspace.path());
    app.handle_event(ApplicationEvent::SessionAttached(other.clone()))
        .unwrap();
    let newer_revision = SessionRevision(snapshot.revision.0 + 1);
    app.handle_event(ApplicationEvent::Managed(
        ManagedEvent::SessionStandingInputsChanged(SessionStandingInputsChanged {
            session_id,
            inputs: SessionStandingInputs {
                pending_questionnaires: vec![second],
                pending_questionnaires_revision: newer_revision,
                ..Default::default()
            },
        }),
    ))
    .unwrap();
    // Reopen a stale snapshot to prove the catalog, not merely reattachment, removed Q1.
    app.handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .unwrap();
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    assert!(
        !rendered_application_rows(&app)
            .join("\n")
            .contains("Obsolete answer")
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireRequestNext);
    // Cycling offers only the remaining live request, even while the snapshot catches up.
    invoke(&mut app, SemanticCommandId::QuestionnaireRequestNext);
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Retained answer")
    );

    snapshot.revision = newer_revision;
    for activity in &mut snapshot.activities {
        if let Activity::Questionnaire {
            questionnaire,
            outcome,
            ..
        } = activity
            && questionnaire.id == first
        {
            *outcome = QuestionnaireOutcome::Withdrawn;
        }
    }
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .unwrap();
    app.handle_event(ApplicationEvent::Managed(
        ManagedEvent::SessionStandingInputsChanged(SessionStandingInputsChanged {
            session_id,
            inputs: SessionStandingInputs {
                pending_questionnaires: vec![],
                pending_questionnaires_revision: SessionRevision(0),
                ..Default::default()
            },
        }),
    ))
    .unwrap();
    assert!(
        rendered_application_rows(&app)
            .join("\n")
            .contains("Retained answer")
    );
    app.handle_event(ApplicationEvent::SessionAttached(other))
        .unwrap();
}
