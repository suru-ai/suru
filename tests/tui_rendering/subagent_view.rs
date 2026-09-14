//! The way into a Subagent's Session and back: pressing its Transcript row
//! opens the child in the Session Content Column, and Escape returns to the
//! parent exactly where the reader left it. The child view offers no path to
//! delivering a Prompt.

use crate::support::{
    buffer_rows, connected_application, failed_session_snapshot, navigable_session_snapshot,
    rendered_application_buffer, rendered_application_rows_at, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityStatus, AgentId, AgentIdentity, AgentSelection, Cost, CostBasis, Message,
        MessageId, MessageRole, MessageStatus, ModelAvailability, ModelId, PromptId, ProviderId,
        Session, SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
        SessionTimestamp, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus, Usage,
        UsageTotal, Workspace,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

/// A parent Session whose Transcript carries one Subagent row, answering with
/// the child Session the row names.
fn parent_with_subagent_row(
    workspace: &std::path::Path,
    status: ActivityStatus,
    duration_ms: Option<u64>,
    turn_in_flight: bool,
) -> (SessionSnapshot, SessionId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the mapping",
        workspace,
    );
    let turn_id = snapshot.turns[0].id;
    if turn_in_flight {
        snapshot.session.status = SessionStatus::Active;
        snapshot.turns[0].status = TurnStatus::Active;
    } else {
        snapshot.turns[0].status = TurnStatus::Completed;
    }
    let child_id = SessionId::new();
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Subagent {
        id: activity_id,
        turn_id,
        status,
        name: "Explore".to_owned(),
        description: "Map the provider seams".to_owned(),
        model: None,
        session_id: child_id,
        duration_ms,
    };
    if turn_in_flight || status == ActivityStatus::Active {
        snapshot.session.working_since = Some(SessionTimestamp::now());
    }
    (snapshot, child_id)
}

#[test]
fn a_subagent_session_header_shows_its_observed_model() {
    let workspace = workspace_dir();
    let mut child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    child.turns[0].agent = Some(AgentIdentity {
        agent: AgentId::new("provider-agent"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("child-model"),
            options: Vec::new(),
        },
    });
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent Session with observed identity");

    let text = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        text.contains("child-model"),
        "the child Session presents its own Model: {text}"
    );
}

/// A Subagent's own Session: parented, running its prompt-less Turn, with the
/// Subagent's narration in its Transcript.
fn child_session_snapshot(
    child_id: SessionId,
    parent_id: SessionId,
    workspace: &std::path::Path,
) -> SessionSnapshot {
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    SessionSnapshot {
        title: String::new(),
        session: Session {
            checkout: None,
            context_fill: None,
            id: child_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Active,
            working_since: Some(SessionTimestamp::now()),
            parent: Some(parent_id),
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: vec![Turn {
            id: turn_id,
            prompt_id: None,
            agent: None,
            status: TurnStatus::Active,
            started_at: None,
            settled_at: None,
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "Mapping the provider seams".to_owned(),
            truncated: false,
            skill_invocations: Vec::new(),
        }],
        activities: Vec::new(),
        transcript: vec![TranscriptItem::Message { message_id }],
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: suru::protocol::SessionRevision(0),
        subagent_usage: None,
        total_cost: None,
    }
}

/// Presses the primary pointer button on the first rendered occurrence of
/// `needle`, the way a reader clicks a row they can see.
fn press_text(
    application: &mut Application,
    width: u16,
    height: u16,
    needle: &str,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, width, height);
    let (column, row) = text_position(&buffer, needle);
    super::support::click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the rendered row")
}

fn press_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press the key")
}

#[test]
fn pressing_a_working_subagent_row_opens_the_childs_session() {
    let workspace = workspace_dir();
    let (snapshot, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, true);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");

    assert_eq!(
        press_text(&mut application, 80, 22, "Explore: Map the provider seams"),
        ApplicationTransition::AttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            child_id,
        )),
        "pressing the row asks to open the child Session it names"
    );
}

#[test]
fn a_settled_subagent_row_in_history_still_opens_the_child() {
    let workspace = workspace_dir();
    let (snapshot, child_id) = parent_with_subagent_row(
        workspace.path(),
        ActivityStatus::Completed,
        Some(12_000),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Subagent settled");
    // The settled Turn folds to its marker, so the reader opens the Turn
    // Folds before the row is theirs to press.
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::TranscriptTurnsToggle,
        )))
        .expect("open the settled Turn Folds");

    assert_eq!(
        press_text(&mut application, 80, 22, "Explore: Map the provider seams"),
        ApplicationTransition::AttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            child_id,
        )),
        "a settled row in scrollback is still the way into the child"
    );
}

#[test]
fn escape_in_a_subagent_session_returns_to_the_parent() {
    let workspace = workspace_dir();
    let parent_id = SessionId::new();
    let child = child_session_snapshot(SessionId::new(), parent_id, workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");
    rendered_application_rows_at(&application, 80, 22);

    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            parent_id,
        )),
        "Escape reports the root parent Viewed while opening it, never interrupting the child"
    );
}

#[test]
fn a_turn_settling_in_an_open_subagent_view_yields_no_viewed_request() {
    let workspace = workspace_dir();
    let child_id = SessionId::new();
    let child = child_session_snapshot(child_id, SessionId::new(), workspace.path());
    let turn_id = child.turns[0].id;
    let revision = SessionRevision(child.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                SessionUpdate {
                    session_id: child_id,
                    revision,
                    changes: vec![SessionChange::TurnStatusChanged {
                        turn_id,
                        status: TurnStatus::Completed,
                        settled_at: Some(SessionTimestamp(100)),
                    }],
                },
            )))
            .expect("settle the Subagent's Turn"),
        ApplicationTransition::Continue,
        "a Subagent view never reports Viewed"
    );
}

#[test]
fn returning_to_the_parent_restores_the_readers_view_state() {
    let workspace = workspace_dir();
    let parent_id = SessionId::new();
    let parent = navigable_session_snapshot(parent_id, workspace.path(), 8);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(parent.clone()))
        .expect("attach the parent Session");
    rendered_application_rows_at(&application, 80, 15);
    for _ in 0..3 {
        application
            .handle_terminal_event(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 40,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }))
            .expect("scroll the parent's Transcript up");
        rendered_application_rows_at(&application, 80, 15);
    }
    let left_at = rendered_application_rows_at(&application, 80, 15);

    let child = child_session_snapshot(SessionId::new(), parent_id, workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("open the Subagent's Session");
    rendered_application_rows_at(&application, 80, 15);
    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            parent_id,
        )),
        "Escape reports the root parent Viewed while asking for it back"
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("return to the parent Session");

    assert_eq!(
        rendered_application_rows_at(&application, 80, 15),
        left_at,
        "the parent comes back exactly where the reader left it"
    );
}

#[test]
fn a_subagent_view_states_its_own_context_and_cost_while_parent_cost_includes_child() {
    let workspace = workspace_dir();
    let (mut parent, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, true);
    let parent_id = parent.session.id;
    parent.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(10_000),
        output_tokens: Some(5_000),
        ..Usage::default()
    });
    parent.session.context_fill = Some(suru::protocol::ContextFill {
        occupied_tokens: 12_400,
        capacity_tokens: Some(200_000),
    });
    parent.turns[0].cost = Cost::from_usd(0.31);
    parent.turns[0].cost_basis = Some(CostBasis::Reported);
    parent.subagent_usage = Some(UsageTotal {
        fresh_input_tokens: Some(4_000),
        output_tokens: Some(1_000),
        cost: Cost::from_usd(0.12),
        ..UsageTotal::default()
    });
    let mut child = child_session_snapshot(child_id, parent_id, workspace.path());
    child.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(4_000),
        output_tokens: Some(1_000),
        ..Usage::default()
    });
    child.session.context_fill = Some(suru::protocol::ContextFill {
        occupied_tokens: 1000,
        capacity_tokens: Some(100_000),
    });
    child.turns[0].cost = Cost::from_usd(0.12);
    child.turns[0].cost_basis = Some(CostBasis::Reported);

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("attach the delegating Session");
    let delegating = buffer_rows(&rendered_application_buffer(&application, 80, 22)).join("\n");
    assert!(
        delegating.contains("12.4K (6%) · $0.43"),
        "the parent states its own work and the Subagent's together: {delegating}"
    );

    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("open the Subagent's Session");
    let delegated = buffer_rows(&rendered_application_buffer(&application, 80, 22)).join("\n");
    assert!(
        delegated.contains("1K (1%) · $0.12"),
        "the child states what it consumed itself: {delegated}"
    );
    assert!(
        !delegated.contains("20K"),
        "and never what its parent consumed: {delegated}"
    );
}

#[test]
fn a_subagent_session_offers_no_path_to_a_prompt() {
    let workspace = workspace_dir();
    let child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");
    rendered_application_rows_at(&application, 80, 22);

    type_terminal_text(&mut application, "hello agent");
    assert_eq!(
        press_key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "Enter delivers nothing from a Subagent's Session"
    );

    let buffer = rendered_application_buffer(&application, 80, 22);
    let text = buffer_rows(&buffer).join("\n");
    assert!(
        !text.contains("hello agent"),
        "typed text lands nowhere — there is no composer to hold it: {text}"
    );
    assert!(
        text.contains("Esc returns to the parent"),
        "the view says how to leave instead of offering a composer: {text}"
    );
    assert!(
        !text.contains("Esc interrupt"),
        "Escape no longer reads as the interrupt gesture: {text}"
    );
    assert!(
        text.contains("Working (") && !text.contains("Esc to interrupt"),
        "the child states its own Working duration without mislabelling Escape: {text}"
    );
}

#[test]
fn a_subagent_session_streams_live_while_attached() {
    let workspace = workspace_dir();
    let child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    let child_id = child.session.id;
    let turn_id = child.turns[0].id;
    let revision = child.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id: child_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::Agent,
                        status: MessageStatus::Completed,
                        content: "Found the orchestration seam".to_owned(),
                        truncated: false,
                        skill_invocations: Vec::new(),
                    },
                }],
            },
        )))
        .expect("stream the Subagent's next message");

    let rows = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        rows.contains("Found the orchestration seam"),
        "the child's Transcript streams live while the Subagent works: {rows}"
    );
}

#[test]
fn child_questionnaire_attention_opens_the_child_panel_and_preserves_parent_and_child_drafts() {
    use suru::protocol::{
        Question, Questionnaire, QuestionnaireId, QuestionnaireOutcome, SubagentInterventions,
    };
    let workspace = workspace_dir();
    let (mut parent, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, false);
    let mut child = child_session_snapshot(child_id, parent.session.id, workspace.path());
    let questionnaire_id = QuestionnaireId::new();
    let activity_id = suru::protocol::ActivityId::new();
    child.activities.push(Activity::Questionnaire {
        id: activity_id,
        turn_id: child.turns[0].id,
        outcome: QuestionnaireOutcome::Pending,
        answer: None,
        questionnaire: Questionnaire {
            id: questionnaire_id,
            questions: vec![Question {
                id: "child-input".into(),
                title: None,
                text: "Which child setting?".into(),
                choices: vec![],
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: false,
                required: true,
            }],
        },
    });
    child
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    child.revision.0 += 1;
    parent.subagent_interventions.push(SubagentInterventions {
        submitting_questionnaires: Vec::new(),
        submitting_approvals: Vec::new(),
        session_id: child_id,
        via_session_id: child_id,
        revision: child.revision,
        pending_questionnaires: vec![questionnaire_id],
        pending_approvals: Vec::new(),
    });
    let mut app = connected_application(workspace.path());
    app.handle_event(ApplicationEvent::SessionAttached(parent.clone()))
        .unwrap();
    type_terminal_text(&mut app, "Parent composer draft");
    let screen = rendered_application_rows_at(&app, 100, 26).join("\n");
    assert!(
        screen.contains("1 Subagent questionnaires pending")
            && !screen.contains("Which child setting?"),
        "{screen}"
    );
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::SubagentBrowse,
    )))
    .unwrap();
    let screen = rendered_application_rows_at(&app, 100, 26).join("\n");
    assert!(screen.contains("1 pending questions"), "{screen}");
    assert!(
        matches!(press_key(&mut app, KeyCode::Enter), ApplicationTransition::AttachSession(reference) if reference.session_id == child_id)
    );
    app.handle_event(ApplicationEvent::SessionAttached(child.clone()))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::QuestionnaireOpen,
    )))
    .unwrap();
    type_terminal_text(&mut app, "Child answer draft");
    press_key(&mut app, KeyCode::Esc);
    assert!(
        matches!(press_key(&mut app, KeyCode::Esc), ApplicationTransition::ViewAndAttachSession(reference) if reference.session_id == parent.session.id)
    );
    app.handle_event(ApplicationEvent::SessionAttached(parent.clone()))
        .unwrap();
    assert!(
        rendered_application_rows_at(&app, 100, 26)
            .join("\n")
            .contains("Parent composer draft")
    );
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::SubagentBrowse,
    )))
    .unwrap();
    press_key(&mut app, KeyCode::Enter);
    app.handle_event(ApplicationEvent::SessionAttached(child.clone()))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::QuestionnaireOpen,
    )))
    .unwrap();
    assert!(
        rendered_application_rows_at(&app, 100, 26)
            .join("\n")
            .contains("Child answer draft")
    );
    press_key(&mut app, KeyCode::Enter);
    assert!(
        matches!(app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(SemanticCommandId::QuestionnaireSubmit))).unwrap(),
        ApplicationTransition::SubmitQuestionnaire { session, id, .. } if session.session_id == child_id && id == questionnaire_id)
    );
    // A root catalog update also discards a hidden child's now-unavailable draft.
    press_key(&mut app, KeyCode::Esc);
    press_key(&mut app, KeyCode::Esc);
    app.handle_event(ApplicationEvent::SessionAttached(parent.clone()))
        .unwrap();
    app.handle_event(ApplicationEvent::Managed(
        suru::managed_client::ManagedEvent::SessionStandingInputsChanged(
            suru::protocol::SessionStandingInputsChanged {
                session_id: parent.session.id,
                inputs: suru::protocol::SessionStandingInputs {
                    subagent_interventions: vec![SubagentInterventions {
                        submitting_questionnaires: Vec::new(),
                        submitting_approvals: Vec::new(),
                        session_id: child_id,
                        via_session_id: child_id,
                        revision: SessionRevision(child.revision.0 + 1),
                        pending_questionnaires: vec![],
                        pending_approvals: Vec::new(),
                    }],
                    ..Default::default()
                },
            },
        ),
    ))
    .unwrap();
    app.handle_event(ApplicationEvent::SessionAttached(child))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::QuestionnaireOpen,
    )))
    .unwrap();
    let screen = rendered_application_rows_at(&app, 100, 26).join("\n");
    assert!(!screen.contains("Child answer draft"));
    assert!(matches!(
        app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::QuestionnaireSubmit
        )))
        .unwrap(),
        ApplicationTransition::Continue
    ));
}

#[test]
fn child_approval_attention_opens_the_owning_session_and_submits_its_decision_there() {
    use suru::protocol::{
        Approval, ApprovalId, ApprovalOutcome, ApprovalSubject, Decision, QuestionnaireId,
        SubagentInterventions,
    };
    let workspace = workspace_dir();
    let (mut parent, child_id) =
        parent_with_subagent_row(workspace.path(), ActivityStatus::Active, None, false);
    let mut child = child_session_snapshot(child_id, parent.session.id, workspace.path());
    let approval_id = ApprovalId::new();
    let activity_id = suru::protocol::ActivityId::new();
    child.activities.push(Activity::Approval {
        id: activity_id,
        turn_id: child.turns[0].id,
        approval: Approval {
            id: approval_id,
            subject: ApprovalSubject::Network {
                host_or_url: "https://child.example.test".into(),
            },
            reason: Some("Fetch child metadata".into()),
        },
        tool_activity_id: None,
        detail_truncated: false,
        outcome: ApprovalOutcome::Pending,
        decision: None,
        follow_up_error: None,
    });
    child
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    child.pending_approvals = vec![approval_id];
    child.pending_approvals_revision = child.revision;
    let question_id = QuestionnaireId::new();
    parent.subagent_interventions.push(SubagentInterventions {
        session_id: child_id,
        via_session_id: child_id,
        revision: child.revision,
        pending_questionnaires: vec![question_id],
        submitting_questionnaires: Vec::new(),
        pending_approvals: vec![approval_id],
        submitting_approvals: Vec::new(),
    });

    let mut app = connected_application(workspace.path());
    app.handle_event(ApplicationEvent::SessionAttached(parent))
        .unwrap();
    let screen = rendered_application_rows_at(&app, 110, 26).join("\n");
    assert!(screen.contains("1 Subagent questionnaire"), "{screen}");
    assert!(screen.contains("1 Approval pending"), "{screen}");
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::SubagentBrowse,
    )))
    .unwrap();
    let screen = rendered_application_rows_at(&app, 110, 26).join("\n");
    assert!(screen.contains("1 pending question"), "{screen}");
    assert!(screen.contains("1 pending Approval"), "{screen}");
    assert!(
        matches!(press_key(&mut app, KeyCode::Enter), ApplicationTransition::AttachSession(reference) if reference.session_id == child_id)
    );
    app.handle_event(ApplicationEvent::SessionAttached(child))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        SemanticCommandId::ApprovalOpen,
    )))
    .unwrap();
    let screen = rendered_application_rows_at(&app, 110, 26).join("\n");
    assert!(screen.contains("https://child.example.test"), "{screen}");
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('3')),
        ApplicationTransition::SubmitDecision {
            session,
            id,
            decision: Decision::Decline,
        } if session.session_id == child_id && id == approval_id
    ));
}

#[test]
fn escape_clears_text_selection_before_leaving_a_subagent() {
    use ratatui::style::Modifier;
    let workspace = workspace_dir();
    let parent_id = SessionId::new();
    let child = child_session_snapshot(SessionId::new(), parent_id, workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 22);
    let (x, y) = text_position(&buffer, "Mapping");
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), x),
        (MouseEventKind::Drag(MouseButton::Left), x + 6),
        (MouseEventKind::Up(MouseButton::Left), x + 6),
    ] {
        application
            .handle_terminal_event(InputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: y,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
    }
    assert!(
        rendered_application_buffer(&application, 80, 22)[(x, y)]
            .modifier
            .contains(Modifier::REVERSED)
    );
    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );
    assert!(
        !rendered_application_buffer(&application, 80, 22)[(x, y)]
            .modifier
            .contains(Modifier::REVERSED)
    );
    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::ViewAndAttachSession(suru::protocol::SessionReference::new(
            suru::protocol::Outlook::Local,
            parent_id
        ))
    );
}

#[test]
fn subagent_view_follows_latest_on_control_end_and_has_no_composer_line_motion() {
    let workspace = workspace_dir();
    let child = child_session_snapshot(SessionId::new(), SessionId::new(), workspace.path());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(child))
        .expect("attach a Subagent's Session");
    for (code, modifiers, command) in [
        (
            KeyCode::End,
            KeyModifiers::CONTROL,
            Some(CommandId::FollowLatest),
        ),
        (KeyCode::End, KeyModifiers::NONE, None),
        (KeyCode::Home, KeyModifiers::NONE, None),
        (KeyCode::Home, KeyModifiers::CONTROL, None),
    ] {
        assert_eq!(
            application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(code, modifiers))),
            command,
        );
    }
}
