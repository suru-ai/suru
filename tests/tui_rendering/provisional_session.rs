//! The Provisional Session: the Session view a client draws from the moment the
//! Landing's first Prompt is submitted until the Server answers with the Session
//! it made, and the refusal it stands through.

use crate::support::{rendered_application_rows, workspace_dir};
use suru::{
    protocol::{
        InitialPrompt, ModelAvailability, Prompt, PromptDelivery, PromptOrder, PromptStatus,
        Session, SessionId, SessionRevision, SessionSnapshot, SessionStatus, SessionTimestamp,
        Workspace,
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
        emoji: None,
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
            status: SessionStatus::Active,
            working_since: Some(working_since),
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
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
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

#[test]
#[ignore]
fn dump_provisional() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let _ = submit_landing_prompt(&mut application, "Rename the widget");
    for row in rendered_application_rows(&application) {
        println!("|{row}|");
    }
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
