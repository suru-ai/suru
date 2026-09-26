//! The Provisional Session: the Session view a client draws from the moment the
//! Landing's first Prompt is submitted until the Server answers with the Session
//! it made, and the refusal it stands through.

use crate::support::{failed_session_snapshot, rendered_application_rows, workspace_dir};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        InitialPrompt, ModelAvailability, Prompt, PromptDelivery, PromptId, PromptOrder,
        PromptStatus, Session, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot,
        SessionStatus, SessionTimestamp, Turn, TurnId, TurnStatus, Workspace,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

/// The Session a Server answers a creation request with under the contract this
/// client is written to: the initial Prompt is admitted and Pending, so the
/// Session is already Working before any Turn exists.
pub fn created_session_snapshot(
    session_id: SessionId,
    prompt: &InitialPrompt,
    workspace: &std::path::Path,
    working_since: SessionTimestamp,
) -> SessionSnapshot {
    SessionSnapshot {
        title: prompt.text.trim().to_owned(),
        icon: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Active,
            working_since: Some(working_since),
            monitoring_since: None,
            parent: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![Prompt {
            id: prompt.id,
            text: prompt.text.clone(),
            skill_invocations: prompt.skill_invocations.clone(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
        }],
        turns: Vec::new(),
        messages: Vec::new(),
        activities: Vec::new(),
        transcript: Vec::new(),
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: suru::protocol::SessionRevision(0),
        subagent_usage: None,
        total_cost: None,
    }
}

fn submit_landing_prompt(application: &mut Application, text: &str) -> InitialPrompt {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            text.to_owned(),
        )))
        .expect("type the Landing Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the Landing Prompt")
    else {
        panic!("a Landing submission should request a Session");
    };
    request.prompt
}

#[test]
fn the_created_session_replaces_the_provisional_one_in_place() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    let session_id = SessionId::new();

    application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            session_id,
            &prompt,
            workspace.path(),
            SessionTimestamp(SessionTimestamp::now().0.saturating_sub(4_000)),
        )))
        .expect("take the created Session");

    let drawn = rendered_application_rows(&application).join("\n");
    // The user row stands where it stood, from the Server's own Pending Prompt
    // rather than from the claim it replaced: still one row, not two.
    assert_eq!(drawn.matches("Rename the widget").count(), 2, "{drawn}");
    // Elapsed time now comes from the Session the Server answered with.
    assert!(drawn.contains("Working (4s"), "{drawn}");
}

#[test]
fn the_landing_abandons_a_provisional_session_and_a_late_creation_leaves_the_route() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("return to the Landing");
    let landing = rendered_application_rows(&application).join("\n");
    assert!(!landing.contains("Rename the widget"), "{landing}");

    application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the late creation");
    let after = rendered_application_rows(&application).join("\n");
    assert_eq!(after, landing, "a late creation must not move the route");
}

#[test]
fn a_refusal_after_leaving_restores_the_landing_draft_and_says_why() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("return to the Landing");

    application
        .handle_event(ApplicationEvent::SessionCreationFailed {
            prompt_id: prompt.id,
            code: None,
            error: "Provider unavailable".to_owned(),
        })
        .expect("take the late refusal");

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("Rename the widget"), "{drawn}");
    assert!(drawn.contains("Provider unavailable"), "{drawn}");
}

/// Refuses the creation the Landing's Prompt asked for, leaving the client
/// standing in the Provisional Session it drew for it.
fn refuse_creation(application: &mut Application, prompt: &InitialPrompt, error: &str) {
    application
        .handle_event(ApplicationEvent::SessionCreationFailed {
            prompt_id: prompt.id,
            code: None,
            error: error.to_owned(),
        })
        .expect("take the refusal");
}

#[test]
fn a_refusal_leaves_the_provisional_session_standing_with_its_prompt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    refuse_creation(&mut application, &prompt, "Provider unavailable");

    let drawn = rendered_application_rows(&application).join("\n");
    // The user Message stays; the refusal stands where the Working Indicator was.
    assert_eq!(drawn.matches("Rename the widget").count(), 2, "{drawn}");
    // Read across the wrap the Transcript's own width gives it.
    let flattened = drawn.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flattened.contains(
            "Error: Could not create Session: Provider unavailable · Enter to retry, or type a new prompt"
        ),
        "{drawn}"
    );
    assert!(!drawn.contains("Working"), "{drawn}");
    // The composer stands empty: Enter there retries the Prompt that was refused.
    assert!(drawn.contains("Type a prompt"), "{drawn}");
}

#[test]
fn an_empty_submit_retries_the_refused_prompt_itself() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    refuse_creation(&mut application, &prompt, "Provider unavailable");

    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("retry the refused Prompt")
    else {
        panic!("an empty submit over a refusal should ask for the Session again");
    };
    assert_eq!(request.prompt.id, prompt.id);
    assert_eq!(request.prompt.text, prompt.text);

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(!drawn.contains("Could not create Session"), "{drawn}");
    assert!(drawn.contains("Working"), "{drawn}");
}

#[test]
fn a_typed_submit_replaces_the_refused_prompt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    refuse_creation(&mut application, &prompt, "Provider unavailable");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Rename the gadget".to_owned(),
        )))
        .expect("type a new Prompt over the refusal");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the replacement")
    else {
        panic!("a typed submit over a refusal should ask for the Session again");
    };
    assert_ne!(request.prompt.id, prompt.id);
    assert_eq!(request.prompt.text, "Rename the gadget");

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(!drawn.contains("Rename the widget"), "{drawn}");
    assert_eq!(drawn.matches("Rename the gadget").count(), 2, "{drawn}");
    assert!(drawn.contains("Working"), "{drawn}");
}

#[test]
fn a_replacement_typed_before_the_refusal_survives_and_can_be_submitted() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Rename the gadget".to_owned(),
        )))
        .expect("type a replacement while creation is in flight");

    refuse_creation(
        &mut application,
        &prompt,
        "Destination Skill `review` is ambiguous",
    );
    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("Rename the gadget"), "{drawn}");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the preserved replacement")
    else {
        panic!("the preserved replacement should retry Session creation")
    };
    assert_ne!(request.prompt.id, prompt.id);
    assert_eq!(request.prompt.text, "Rename the gadget");
}

#[test]
fn a_destination_skill_refusal_restores_the_original_prompt_for_editing() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Use $review here");
    application
        .handle_event(ApplicationEvent::SessionCreationFailed {
            prompt_id: prompt.id,
            code: Some(SessionErrorCode::InvalidSkillInvocation),
            error: "Destination Skill `review` is ambiguous".to_owned(),
        })
        .expect("take the destination Skill refusal");

    let drawn = rendered_application_rows(&application).join("\n");
    assert_eq!(drawn.matches("Use $review here").count(), 3, "{drawn}");
    let ApplicationTransition::CreateSession(retry) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the editable original Prompt")
    else {
        panic!("the restored Skill Prompt should retry creation")
    };
    assert_eq!(retry.prompt.id, prompt.id);
    assert_eq!(retry.prompt.text, prompt.text);
}

#[test]
fn leaving_a_refused_provisional_session_keeps_its_text_as_the_landing_draft() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    refuse_creation(&mut application, &prompt, "Provider unavailable");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("return to the Landing");
    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("Rename the widget"), "{drawn}");

    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the restored draft")
    else {
        panic!("the restored draft should ask for the Session again");
    };
    assert_eq!(request.prompt.id, prompt.id);
}

fn press_escape(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("press Escape")
}

#[test]
fn escape_in_a_provisional_session_arms_an_interrupt_that_waits_for_the_session() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    assert_eq!(
        press_escape(&mut application),
        ApplicationTransition::Continue
    );
    let armed = rendered_application_rows(&application).join("\n");
    assert!(armed.contains("Esc again to interrupt"), "{armed}");

    // Confirming with no Session to send it to records the intent instead.
    assert_eq!(
        press_escape(&mut application),
        ApplicationTransition::Continue
    );

    let session_id = SessionId::new();
    let transition = application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            session_id,
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the created Session");
    let ApplicationTransition::InterruptSession { session } = transition else {
        panic!("a confirmed interrupt should travel with the Session's arrival: {transition:?}");
    };
    assert_eq!(session.session_id, session_id);
}

#[test]
fn un_arming_an_interrupt_drops_the_intent() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    press_escape(&mut application);
    application
        .handle_event(ApplicationEvent::Command(CommandId::CloseCommandMode))
        .expect("un-arm the interrupt");

    let transition = application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the created Session");
    assert_eq!(transition, ApplicationTransition::Continue);
}

#[test]
fn a_session_working_only_for_an_undelivered_prompt_can_be_interrupted() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    let session_id = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            session_id,
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the created Session");

    assert_eq!(
        press_escape(&mut application),
        ApplicationTransition::Continue
    );
    let armed = rendered_application_rows(&application).join("\n");
    assert!(armed.contains("again to interrupt"), "{armed}");
    let transition = press_escape(&mut application);
    let ApplicationTransition::InterruptSession { session } = transition else {
        panic!("interrupting a Session owed to a Prompt should reach it: {transition:?}");
    };
    assert_eq!(session.session_id, session_id);

    // The Server withdraws the Prompt rather than stopping a Turn it never
    // began, and the text comes back to this client's composer.
    let mut withdrawn = created_session_snapshot(
        session_id,
        &prompt,
        workspace.path(),
        SessionTimestamp::now(),
    );
    withdrawn.session.working_since = None;
    withdrawn.session.status = SessionStatus::Idle;
    withdrawn.prompts[0].status = PromptStatus::Cancelled;
    withdrawn.revision = SessionRevision(withdrawn.revision.0 + 1);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(withdrawn)))
        .expect("take the withdrawal");

    let drawn = rendered_application_rows(&application).join("\n");
    // The Title and the composer say it; the Transcript no longer does.
    assert_eq!(drawn.matches("Rename the widget").count(), 2, "{drawn}");
    assert!(!drawn.contains("Working"), "{drawn}");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the returned text")
    else {
        panic!("the withdrawn Prompt should stand in this Session's composer");
    };
    assert_eq!(request.prompt.text, prompt.text);
    assert_eq!(request.prompt.id, prompt.id);
}

#[test]
fn a_draft_typed_while_the_server_answers_is_kept_and_migrates_onto_the_session() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "A later thought".to_owned(),
        )))
        .expect("type while the Server answers");
    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("A later thought"), "{drawn}");
    // Nothing is delivered until the Session arrives.
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit while the Server answers"),
        ApplicationTransition::Continue
    );

    application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the created Session");

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("A later thought"), "{drawn}");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the migrated draft")
    else {
        panic!("the draft should have migrated onto the created Session's composer");
    };
    assert_eq!(request.prompt.text, "A later thought");
}

#[test]
fn a_refusal_waits_for_the_landing_while_another_session_is_open() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Another Session",
            workspace.path(),
        )))
        .expect("open another Session");

    refuse_creation(&mut application, &prompt, "Provider unavailable");
    let opened = rendered_application_rows(&application).join("\n");
    assert!(!opened.contains("Provider unavailable"), "{opened}");
    assert!(!opened.contains("Rename the widget"), "{opened}");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("return to the Landing");
    let landing = rendered_application_rows(&application).join("\n");
    assert!(landing.contains("Provider unavailable"), "{landing}");
    assert!(landing.contains("Rename the widget"), "{landing}");
}

#[test]
fn landing_submit_draws_the_provisional_session_at_once() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    let drawn = rendered_application_rows(&application).join("\n");
    assert_eq!(prompt.text, "Rename the widget");
    // The Prompt stands as the user Message it will become, and the Title the
    // Prompt gives it heads the view.
    assert_eq!(drawn.matches("Rename the widget").count(), 2);
    // Working, with interruption guidance but no elapsed time: only the Server
    // knows when Working began.
    assert!(drawn.contains("Working"), "{drawn}");
    assert!(drawn.contains("Esc to interrupt"), "{drawn}");
    assert!(!drawn.contains("0s"), "{drawn}");
    // The composer stands empty and ready for a draft.
    assert!(drawn.contains("Type a prompt"), "{drawn}");
}

/// A steer another client admitted, or this one did: Pending and owed a Turn
/// unless `turn` names one that has taken it.
fn admitted_steer(text: &str, order: u64, id: PromptId) -> Prompt {
    Prompt {
        id,
        text: text.to_owned(),
        skill_invocations: Vec::new(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(order),
        status: PromptStatus::Pending,
    }
}

/// A Turn that has taken the Prompt it names. Whatever became of it, that
/// Prompt is no longer one the Session is waiting to deliver.
fn turn_for(prompt_id: PromptId) -> Turn {
    Turn {
        id: TurnId::new(),
        prompt_id: Some(prompt_id),
        agent: None,
        status: TurnStatus::Completed,
        started_at: Some(SessionTimestamp::now()),
        settled_at: Some(SessionTimestamp::now()),
        usage: None,
        cost: None,
        cost_basis: None,
        cost_details: None,
        last_output_at: None,
    }
}

/// Opens a Session that is Working for the Prompts given, as a Server under
/// this contract reports one: Working from admission, with no Turn unless the
/// fixture names one.
fn open_working_session(
    application: &mut Application,
    workspace: &std::path::Path,
    prompts: Vec<Prompt>,
    turns: Vec<Turn>,
) -> (SessionId, SessionSnapshot) {
    let session_id = SessionId::new();
    let mut snapshot = created_session_snapshot(
        session_id,
        &InitialPrompt {
            id: PromptId::new(),
            text: "Held Session".to_owned(),
            skill_invocations: Vec::new(),
        },
        workspace,
        SessionTimestamp::now(),
    );
    snapshot.prompts = prompts;
    snapshot.turns = turns;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("open the Working Session");
    (session_id, snapshot)
}

/// The same Session with every Prompt withdrawn, as it reads once an interrupt
/// has cancelled what it was Working for.
fn withdrawn(mut snapshot: SessionSnapshot) -> SessionSnapshot {
    for prompt in &mut snapshot.prompts {
        if prompt.status == PromptStatus::Pending {
            prompt.status = PromptStatus::Cancelled;
        }
    }
    snapshot.session.working_since = None;
    snapshot.session.status = SessionStatus::Idle;
    snapshot.revision = SessionRevision(snapshot.revision.0 + 1);
    snapshot
}

fn composer_holds(application: &mut Application) -> Option<InitialPrompt> {
    match application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit whatever the composer holds")
    {
        ApplicationTransition::AdmitPrompt { request, .. } => Some(request.prompt),
        _ => None,
    }
}

#[test]
fn only_the_prompt_owed_a_turn_comes_back_from_an_interrupt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let delivering = PromptId::new();
    let owed = PromptId::new();
    let (_, snapshot) = open_working_session(
        &mut application,
        workspace.path(),
        vec![
            admitted_steer("Already taken by a Turn", 0, delivering),
            admitted_steer("Still owed a Turn", 1, owed),
        ],
        vec![turn_for(delivering)],
    );

    press_escape(&mut application);
    press_escape(&mut application);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            withdrawn(snapshot),
        )))
        .expect("take the withdrawal");

    let returned = composer_holds(&mut application).expect("a withdrawn Prompt comes back");
    assert_eq!(returned.id, owed);
    assert_eq!(returned.text, "Still owed a Turn");
}

#[test]
fn two_prompts_owed_a_turn_return_the_earliest_and_never_overwrite_it() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let earliest = PromptId::new();
    let (_, snapshot) = open_working_session(
        &mut application,
        workspace.path(),
        vec![
            admitted_steer("Asked first", 0, earliest),
            admitted_steer("Asked second", 1, PromptId::new()),
        ],
        Vec::new(),
    );

    press_escape(&mut application);
    press_escape(&mut application);
    // Asked again while the Session is still Working for it, which owes the
    // reader that one Prompt rather than a second copy of it.
    press_escape(&mut application);
    press_escape(&mut application);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            withdrawn(snapshot),
        )))
        .expect("take the withdrawal");

    let returned = composer_holds(&mut application).expect("a withdrawn Prompt comes back");
    assert_eq!(returned.id, earliest);
    assert_eq!(returned.text, "Asked first");
}

#[test]
fn a_withdrawal_never_overwrites_a_draft_the_reader_is_writing() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (_, snapshot) = open_working_session(
        &mut application,
        workspace.path(),
        vec![admitted_steer("The withdrawn ask", 0, PromptId::new())],
        Vec::new(),
    );

    press_escape(&mut application);
    press_escape(&mut application);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Something else entirely".to_owned(),
        )))
        .expect("write a draft while the withdrawal travels");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            withdrawn(snapshot),
        )))
        .expect("take the withdrawal");

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(drawn.contains("Something else entirely"), "{drawn}");
    let held = composer_holds(&mut application).expect("the draft is still submittable");
    assert_eq!(held.text, "Something else entirely");
}

#[test]
fn leaving_a_session_gives_up_the_withdrawal_it_was_owed() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (_, snapshot) = open_working_session(
        &mut application,
        workspace.path(),
        vec![admitted_steer("The withdrawn ask", 0, PromptId::new())],
        Vec::new(),
    );

    press_escape(&mut application);
    press_escape(&mut application);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("leave for the Landing");
    // Coming back to it, the reader has written something of their own.
    application
        .handle_event(ApplicationEvent::SessionAttached(withdrawn(snapshot)))
        .expect("open it again, now withdrawn");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "A fresh ask".to_owned(),
        )))
        .expect("write a fresh ask");

    let held = composer_holds(&mut application).expect("the fresh ask is submittable");
    assert_eq!(held.text, "A fresh ask");
}

#[test]
fn a_confirmed_interrupt_says_so_until_the_session_takes_it() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");

    press_escape(&mut application);
    press_escape(&mut application);
    let confirmed = rendered_application_rows(&application).join("\n");
    assert!(confirmed.contains("interrupting…"), "{confirmed}");
    assert!(!confirmed.contains("to interrupt"), "{confirmed}");

    // Escape again asks for nothing new and duplicates nothing.
    press_escape(&mut application);
    let again = rendered_application_rows(&application).join("\n");
    assert_eq!(again, confirmed);

    let transition = application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the created Session");
    assert!(
        matches!(transition, ApplicationTransition::InterruptSession { .. }),
        "{transition:?}"
    );
}

#[test]
fn a_session_carrying_another_prompt_neither_replaces_the_claim_nor_takes_its_interrupt() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let prompt = submit_landing_prompt(&mut application, "Rename the widget");
    press_escape(&mut application);
    press_escape(&mut application);

    let stranger = InitialPrompt {
        id: PromptId::new(),
        text: "Someone else's ask".to_owned(),
        skill_invocations: Vec::new(),
    };
    let transition = application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &stranger,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take a creation that answers another Prompt");
    assert_eq!(transition, ApplicationTransition::Continue);

    let drawn = rendered_application_rows(&application).join("\n");
    assert!(!drawn.contains("Someone else's ask"), "{drawn}");
    assert!(drawn.contains("Rename the widget"), "{drawn}");
    assert!(drawn.contains("interrupting…"), "{drawn}");

    // The claim's own Session still takes the interrupt it is owed.
    let transition = application
        .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
            SessionId::new(),
            &prompt,
            workspace.path(),
            SessionTimestamp::now(),
        )))
        .expect("take the claim's own Session");
    assert!(
        matches!(transition, ApplicationTransition::InterruptSession { .. }),
        "{transition:?}"
    );
}

// The Aside beside a Provisional Session: it appears the moment the first
// Prompt is submitted, answering for the claim with the one entry the client
// can draw, and the real Session's tree replaces that entry in place.

mod aside {
    use super::{created_session_snapshot, submit_landing_prompt};
    use crate::support::{
        connected_application, deliver_settings, rendered_application_rows_at, workspace_dir,
    };
    use suru::{
        managed_client::SubagentTreeEvent,
        protocol::{
            ActivityStatus, EffectiveSettings, Outlook, SessionId, SessionReference,
            SessionTimestamp, SidebarVisibility, SubagentTreeEntry, SubagentTreeRevision,
            SubagentTreeSnapshot, SubagentTreeTopLevel,
        },
        tui::{Application, ApplicationEvent, CommandId, SemanticCommandId},
    };

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 24;
    /// The launch width of the Aside, rule included.
    const ASIDE_WIDTH: u16 = 32;

    /// A connected client on the Landing whose Settings show the Aside, with
    /// the Sidebar kept off the frame, and a presentation clock the test
    /// moves by hand so the Aside's Loading quiet period passes unwaited.
    fn landing(
        workspace: &std::path::Path,
    ) -> (
        Application,
        std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    ) {
        let now = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let clock = std::sync::Arc::clone(&now);
        let mut application = connected_application(workspace)
            .with_presentation_clock(move || *clock.lock().expect("read the presentation clock"));
        let mut settings = EffectiveSettings::default();
        settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
        deliver_settings(&mut application, settings);
        (application, now)
    }

    fn advance(now: &std::sync::Mutex<std::time::Instant>, milliseconds: u64) {
        *now.lock().expect("advance the presentation clock") +=
            std::time::Duration::from_millis(milliseconds);
    }

    /// Whether the frame gives the Aside its columns: its rule stands down the
    /// left of them on every row.
    fn aside_is_drawn(application: &Application) -> bool {
        rendered_application_rows_at(application, WIDTH, HEIGHT)
            .iter()
            .all(|row| row.chars().nth(usize::from(WIDTH - ASIDE_WIDTH)) == Some('│'))
    }

    /// The Aside's own content columns of its first rows.
    fn aside_rows(application: &Application) -> Vec<String> {
        rendered_application_rows_at(application, WIDTH, HEIGHT)
            .iter()
            .take(4)
            .map(|row| {
                row.chars()
                    .skip(usize::from(WIDTH - ASIDE_WIDTH + 2))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn tree(top: SessionId, subagent: SessionId) -> SubagentTreeSnapshot {
        SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                session_id: top,
                title: "Rename the widget".to_owned(),
                working_since: None,
                needs_intervention: false,
            },
            subagents: vec![SubagentTreeEntry {
                session_id: subagent,
                parent_session_id: top,
                spawn_order: 0,
                name: "Explore".to_owned(),
                title: "Find the widget".to_owned(),
                status: ActivityStatus::Active,
                worked_ms: Some(0),
                working_since: None,
                needs_intervention: false,
            }],
        }
    }

    const PROVISIONAL_ENTRY: [&str; 3] = ["Subagents 0", "⠋ Rename the widget", ""];

    #[test]
    fn the_aside_appears_at_submission_with_the_provisional_sessions_one_entry() {
        let workspace = workspace_dir();
        let (mut application, _now) = landing(workspace.path());
        assert!(
            !aside_is_drawn(&application),
            "the Landing has no Session for the Aside to answer for"
        );

        submit_landing_prompt(&mut application, "Rename the widget");

        assert!(
            aside_is_drawn(&application),
            "the Aside appears the moment the first Prompt is submitted"
        );
        assert_eq!(
            aside_rows(&application)[..3],
            PROVISIONAL_ENTRY,
            "with one entry: the Title the Prompt gives and the Working Marker, and no \
             time, since only the Server knows when Working began"
        );
    }

    #[test]
    fn the_real_sessions_tree_replaces_the_provisional_entry_in_place() {
        let workspace = workspace_dir();
        let (mut application, now) = landing(workspace.path());
        let prompt = submit_landing_prompt(&mut application, "Rename the widget");
        let top = SessionId::new();

        application
            .handle_event(ApplicationEvent::SessionCreated(created_session_snapshot(
                top,
                &prompt,
                workspace.path(),
                SessionTimestamp::now(),
            )))
            .expect("take the created Session");
        for waited in [0, 300, 1_000] {
            advance(&now, waited);
            assert!(aside_is_drawn(&application), "the Aside never leaves");
            assert_eq!(
                aside_rows(&application)[..3],
                PROVISIONAL_ENTRY,
                "until its tree lands, the Session carries on the entry the claim stood \
                 in — neither blank nor Loading, however long the tree takes"
            );
        }

        let explore = SessionId::new();
        application
            .handle_event(ApplicationEvent::SubagentTree {
                through: SessionReference::new(Outlook::Local, top),
                event: SubagentTreeEvent::Snapshot(tree(top, explore)),
            })
            .expect("take the Session's tree");

        assert_eq!(
            aside_rows(&application)[..3],
            [
                "Subagents 1",
                "Rename the widget",
                "└ ⠋ Explore Find the widget"
            ],
            "the tree replaces the entry in place"
        );
    }

    #[test]
    fn returning_to_the_landing_takes_the_aside_with_the_provisional_session() {
        let workspace = workspace_dir();
        let (mut application, _now) = landing(workspace.path());
        submit_landing_prompt(&mut application, "Rename the widget");

        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SessionNew,
            )))
            .expect("abandon the Provisional Session for the Landing");

        assert!(
            !aside_is_drawn(&application),
            "the Aside behaves as the Landing does"
        );
    }

    #[test]
    fn a_refused_provisional_session_keeps_its_entry_until_the_reader_leaves_it() {
        let workspace = workspace_dir();
        let (mut application, _now) = landing(workspace.path());
        let prompt = submit_landing_prompt(&mut application, "Rename the widget");

        super::refuse_creation(&mut application, &prompt, "Provider unavailable");

        assert_eq!(
            aside_rows(&application)[..2],
            ["Subagents 0", "Rename the widget"],
            "the refused view stands, and its entry is no longer Working"
        );

        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SessionNew,
            )))
            .expect("leave the refused Provisional Session");
        assert!(
            !aside_is_drawn(&application),
            "leaving it for the Landing takes the Aside away"
        );
    }
}
