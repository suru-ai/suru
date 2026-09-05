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
            combine_freeform: false,
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
                combine_freeform: false,
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
                combine_freeform: false,
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
            combine_freeform: false,
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
        expanded.contains("Which build target?") && expanded.contains("Remote"),
        "{expanded}"
    );
}

#[test]
fn batch_navigation_retains_edits_and_reviews_supported_multiple_selections_and_omission() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let choice = |id: &str, recommended| QuestionChoice {
        id: id.into(),
        label: id.into(),
        description: Some(format!("About {id}")),
        recommended,
    };
    let questionnaire = Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![
            Question {
                id: "environment".into(),
                title: Some("Environment".into()),
                text: "Which environment?".into(),
                choices: vec![choice("Local", true), choice("Remote", false)],
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: false,
                required: true,
            },
            Question {
                id: "checks".into(),
                title: Some("Checks".into()),
                text: "Which checks?".into(),
                choices: vec![choice("Unit", false), choice("Integration", false)],
                multiple: true,
                freeform: true,
                combine_freeform: true,
                secret: false,
                required: true,
            },
            Question {
                id: "note".into(),
                title: None,
                text: "Any optional note?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: false,
                required: false,
            },
        ],
    };
    let id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id,
        turn_id,
        questionnaire: questionnaire.clone(),
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
    invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
    let first = crate::support::rendered_application_rows_at(&app, 80, 28).join("\n");
    assert!(
        first.contains("Question 1 of 3")
            && first.contains("[ ] Local (recommended)")
            && first.contains("About Local")
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireOmit);
    key(&mut app, KeyCode::Enter);
    assert!(
        crate::support::rendered_application_rows_at(&app, 80, 28)
            .join("\n")
            .contains("Question 1 requires a supported answer")
    );
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Tab);
    assert!(
        crate::support::rendered_application_rows_at(&app, 80, 28)
            .join("\n")
            .contains("Question 2 of 3")
    );
    key(&mut app, KeyCode::Enter);
    assert!(
        crate::support::rendered_application_rows_at(&app, 80, 28)
            .join("\n")
            .contains("Question 2 requires a supported answer")
    );
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Char(' '));
    type_terminal_text(&mut app, "Extra lint");
    key(&mut app, KeyCode::BackTab);
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Tab);
    let second = crate::support::rendered_application_rows_at(&app, 80, 28).join("\n");
    assert!(
        second.contains("[x] Unit")
            && second.contains("[x] Integration")
            && second.contains("Additional text: Extra lint"),
        "{second}"
    );
    key(&mut app, KeyCode::Down); // Choice navigation switches from text editing without dropping its draft.
    key(&mut app, KeyCode::Char(' ')); // Remove Integration explicitly.
    key(&mut app, KeyCode::Tab);
    assert!(
        crate::support::rendered_application_rows_at(&app, 80, 28)
            .join("\n")
            .contains("Ctrl+O omit")
    );
    invoke(&mut app, SemanticCommandId::QuestionnaireOmit);
    key(&mut app, KeyCode::Enter);
    let review = crate::support::rendered_application_rows_at(&app, 80, 28).join("\n");
    assert!(
        review.contains("Review Answer")
            && review.contains("Which environment?")
            && review.contains("Remote")
            && review.contains("Which checks?")
            && review.contains("Unit; Extra lint")
            && review.contains("Any optional note?"),
        "{review}"
    );
    let ApplicationTransition::SubmitQuestionnaire {
        submission: QuestionnaireSubmission::Answer { answer },
        ..
    } = invoke(&mut app, SemanticCommandId::QuestionnaireSubmit)
    else {
        panic!("whole batch submitted after review")
    };
    assert_eq!(
        answer.questions,
        vec![
            QuestionAnswer::Selected {
                choices: vec!["Remote".into()]
            },
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["Unit".into()],
                text: "Extra lint".into()
            },
            QuestionAnswer::Omitted
        ]
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
    invoke(&mut app, SemanticCommandId::TranscriptFoldsToggle);
    let history = crate::support::rendered_application_rows_at(&app, 80, 28).join("\n");
    assert!(
        history.contains("3 question(s)")
            && history.contains("Which environment?")
            && history.contains("Which checks?")
            && history.contains("Any optional note?")
            && history.contains("Unit; Extra lint"),
        "{history}"
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
                combine_freeform: false,
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
                submitting_questionnaires: Vec::new(),
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
                submitting_questionnaires: Vec::new(),
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

#[test]
fn another_clients_acceptance_disables_private_drafts_then_discards_them_on_settlement() {
    use suru::protocol::{SessionChange, SessionRevision, SessionUpdate};
    let workspace = workspace_dir();
    let mut first = connected_application(workspace.path());
    let mut second = connected_application(workspace.path());
    let (session_id, mut snapshot, turn_id) = enter_active_session(&mut first, workspace.path());
    enter_active_session(&mut second, workspace.path());
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id,
        questionnaire: Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![Question {
                id: "target".into(),
                title: None,
                text: "Which target?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                combine_freeform: false,
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
    for (app, draft) in [
        (&mut first, "First private draft"),
        (&mut second, "Second private draft"),
    ] {
        app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .unwrap();
        type_terminal_text(app, "Keep composer");
        invoke(app, SemanticCommandId::QuestionnaireOpen);
        type_terminal_text(app, draft);
        assert!(rendered_application_rows(app).join("\n").contains(draft));
    }
    assert!(
        !rendered_application_rows(&second)
            .join("\n")
            .contains("First private draft")
    );
    assert!(
        !rendered_application_rows(&first)
            .join("\n")
            .contains("Second private draft")
    );
    let accepted = SessionUpdate {
        session_id,
        revision: SessionRevision(snapshot.revision.0 + 1),
        changes: vec![SessionChange::QuestionnaireAccepted { activity_id }],
    };
    for app in [&mut first, &mut second] {
        app.handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            accepted.clone(),
        )))
        .unwrap();
        assert!(matches!(
            invoke(app, SemanticCommandId::QuestionnaireSubmit),
            ApplicationTransition::Continue
        ));
        let screen = rendered_application_rows(app).join("\n");
        assert!(
            screen.contains("Submitting") && screen.contains("private draft"),
            "{screen}"
        );
        assert!(matches!(
            invoke(app, SemanticCommandId::QuestionnaireDecline),
            ApplicationTransition::Continue
        ));
        invoke(app, SemanticCommandId::QuestionnaireHide);
        assert!(
            rendered_application_rows(app)
                .join("\n")
                .contains("Keep composer")
        );
        invoke(app, SemanticCommandId::QuestionnaireOpen);
        assert!(
            !rendered_application_rows(app)
                .join("\n")
                .contains("private draft")
        );
    }
    let settled = SessionUpdate {
        session_id,
        revision: SessionRevision(snapshot.revision.0 + 2),
        changes: vec![SessionChange::QuestionnaireSettled {
            activity_id,
            outcome: QuestionnaireOutcome::Answered,
            answer: Some(Answer {
                questions: vec![QuestionAnswer::Freeform {
                    text: "First private draft".into(),
                }],
            }),
        }],
    };
    for app in [&mut first, &mut second] {
        app.handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            settled.clone(),
        )))
        .unwrap();
        assert!(
            rendered_application_rows(app)
                .join("\n")
                .contains("Answered")
        );
    }
    // Exiting a Client drops unsent edits. A fresh Client viewing the same live
    // snapshot starts an empty local Answer while the server request stays pending.
    drop(first);
    let mut reconnected = connected_application(workspace.path());
    enter_active_session(&mut reconnected, workspace.path());
    reconnected
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    invoke(&mut reconnected, SemanticCommandId::QuestionnaireOpen);
    let screen = rendered_application_rows(&reconnected).join("\n");
    assert!(!screen.contains("private draft"), "{screen}");
    assert!(matches!(
        invoke(&mut reconnected, SemanticCommandId::QuestionnaireSubmit),
        ApplicationTransition::Continue
    ));
}

#[test]
fn submission_reconciliation_preserves_rejected_drafts_and_disables_unconfirmed_or_consumed_answers()
 {
    for terminal in [
        QuestionnaireOutcome::DeliveryUncertain,
        QuestionnaireOutcome::Answered,
    ] {
        let workspace = workspace_dir();
        let mut app = connected_application(workspace.path());
        let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
        let activity_id = ActivityId::new();
        let questionnaire = Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![Question {
                id: "q".into(),
                title: None,
                text: "What should I use?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: false,
                required: true,
            }],
        };
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
        invoke(&mut app, SemanticCommandId::QuestionnaireOpen);
        type_terminal_text(&mut app, "Preserve my work");
        invoke(&mut app, SemanticCommandId::QuestionnaireReview);
        assert!(matches!(
            invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
            ApplicationTransition::SubmitQuestionnaire { .. }
        ));
        let session = suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            snapshot.session.id,
        );
        app.handle_event(ApplicationEvent::QuestionnaireSubmissionReconciled {
            id: questionnaire.id,
            session: session.clone(),
            snapshot: None,
            error: Some(
                "Submission status is unconfirmed. Reconnect to check before retrying.".into(),
            ),
        })
        .unwrap();
        assert!(matches!(
            invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
            ApplicationTransition::Continue
        ));
        assert!(matches!(
            invoke(&mut app, SemanticCommandId::QuestionnaireDecline),
            ApplicationTransition::Continue
        ));
        assert!(
            rendered_application_rows(&app)
                .join("\n")
                .contains("Preserve my work")
        );
        for state in [
            QuestionnaireOutcome::Submitting,
            QuestionnaireOutcome::SubmissionRejected,
        ] {
            if let Activity::Questionnaire { outcome, .. } = snapshot.activities.last_mut().unwrap()
            {
                *outcome = state;
            }
            snapshot.revision.0 += 1;
            app.handle_event(ApplicationEvent::QuestionnaireSubmissionReconciled {
                id: questionnaire.id,
                session: session.clone(),
                snapshot: Some(snapshot.clone()),
                error: None,
            })
            .unwrap();
        }
        let screen = rendered_application_rows(&app).join("\n");
        assert!(
            screen.contains("Preserve my work") && screen.contains("not delivered"),
            "{screen}"
        );
        assert!(
            matches!(invoke(&mut app, SemanticCommandId::QuestionnaireSubmit), ApplicationTransition::SubmitQuestionnaire { submission: QuestionnaireSubmission::Answer { answer }, .. } if answer.questions == vec![QuestionAnswer::Freeform { text: "Preserve my work".into() }])
        );
        let rejected = snapshot.clone();
        if let Activity::Questionnaire { outcome, .. } = snapshot.activities.last_mut().unwrap() {
            *outcome = terminal;
        }
        snapshot.revision.0 += 1;
        app.handle_event(ApplicationEvent::QuestionnaireSubmissionReconciled {
            id: questionnaire.id,
            session: session.clone(),
            snapshot: Some(snapshot),
            error: Some("Provider delivery is uncertain. This Answer will not be resent.".into()),
        })
        .unwrap();
        let screen = rendered_application_rows(&app).join("\n");
        assert!(
            !screen.contains("Preserve my work")
                && screen.contains(if terminal == QuestionnaireOutcome::Answered {
                    "Answered"
                } else {
                    "uncertain"
                }),
            "{screen}"
        );
        assert!(matches!(
            invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
            ApplicationTransition::Continue
        ));
        if terminal == QuestionnaireOutcome::Answered {
            for stale in [Some(rejected), None] {
                app.handle_event(ApplicationEvent::QuestionnaireSubmissionReconciled {
                    id: questionnaire.id,
                    session: session.clone(),
                    snapshot: stale,
                    error: Some("Stale rejection must not replace accepted delivery".into()),
                })
                .unwrap();
                let screen = rendered_application_rows(&app).join("\n");
                assert!(
                    screen.contains("Answered") && !screen.contains("Stale rejection"),
                    "{screen}"
                );
                assert!(matches!(
                    invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
                    ApplicationTransition::Continue
                ));
            }
        }
    }
}

#[test]
fn secret_questionnaire_masks_editing_and_review_but_submits_the_original_value_then_discards_it() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "Ordinary composer draft");
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id,
        questionnaire: Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![Question {
                id: "token".into(),
                title: Some("Credential".into()),
                text: "Access token?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                combine_freeform: true,
                secret: true,
                required: false,
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
    const SECRET: &str = "private credential value";
    type_terminal_text(&mut app, SECRET);
    let editing = rendered_application_rows(&app).join("\n");
    assert!(
        !editing.contains(SECRET) && editing.contains("••••••••"),
        "{editing}"
    );
    key(&mut app, KeyCode::Enter);
    let review = rendered_application_rows(&app).join("\n");
    assert!(
        !review.contains(SECRET) && review.contains("••••••••"),
        "{review}"
    );
    let transition = invoke(&mut app, SemanticCommandId::QuestionnaireSubmit);
    assert!(!format!("{transition:?}").contains(SECRET));
    let ApplicationTransition::SubmitQuestionnaire {
        submission: QuestionnaireSubmission::Answer { answer },
        ..
    } = transition
    else {
        panic!("secret is explicitly submitted after review")
    };
    assert_eq!(
        answer.questions,
        vec![QuestionAnswer::Freeform {
            text: SECRET.into()
        }]
    );
    let Activity::Questionnaire {
        outcome, answer, ..
    } = snapshot.activities.last_mut().unwrap()
    else {
        unreachable!()
    };
    *outcome = QuestionnaireOutcome::Answered;
    *answer = Some(Answer {
        questions: vec![QuestionAnswer::SecretAnswered],
    });
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .unwrap();
    let screen = rendered_application_rows(&app).join("\n");
    assert!(
        screen.contains("Ordinary composer draft") && !screen.contains(SECRET),
        "{screen}"
    );
    assert!(matches!(
        invoke(&mut app, SemanticCommandId::QuestionnaireSubmit),
        ApplicationTransition::Continue
    ));
}
