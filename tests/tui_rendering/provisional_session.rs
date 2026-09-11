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
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId},
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
