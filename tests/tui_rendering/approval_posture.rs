use crate::support::{
    connected_application, rendered_application_rows_at, selected_session_snapshot,
    type_terminal_text, workspace_dir,
};
use suru::{
    protocol::{
        AgentSelection, ApprovalPosture, CodexApprovalPolicy, CodexSandboxMode, ModelId,
        ProviderId, SessionApprovalPosture, SessionId,
    },
    tui::{ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

#[test]
fn session_chrome_cycle_and_picker_use_the_typed_approval_posture_without_losing_the_draft() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let session_id = SessionId::new();
    let mut snapshot = selected_session_snapshot(
        session_id,
        workspace.path(),
        AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-test"),
            options: Vec::new(),
        },
    );
    snapshot.session.approval_posture = Some(SessionApprovalPosture {
        value: ApprovalPosture::Codex {
            approval_policy: CodexApprovalPolicy::OnRequest,
            sandbox_mode: CodexSandboxMode::WorkspaceWrite,
        },
        pinned: true,
    });
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    type_terminal_text(&mut application, "draft survives posture changes");

    let frame = rendered_application_rows_at(&application, 80, 24).join("\n");
    assert!(frame.contains("Approval Posture:"), "{frame}");
    assert!(frame.contains("on-request"), "{frame}");
    assert!(frame.contains("workspace-write"), "{frame}");

    let transition = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ApprovalPostureCycle,
        )))
        .unwrap();
    assert_eq!(
        transition,
        ApplicationTransition::UpdateApprovalPosture {
            session: suru::protocol::SessionReference::new(
                suru::protocol::Outlook::Local,
                session_id
            ),
            request: suru::protocol::UpdateApprovalPostureRequest {
                posture: Some(ApprovalPosture::Codex {
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox_mode: CodexSandboxMode::WorkspaceWrite,
                }),
            },
        }
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ApprovalPostureOpen,
        )))
        .unwrap();
    let picker = rendered_application_rows_at(&application, 80, 24).join("\n");
    for choice in [
        "Approval policy: untrusted",
        "Approval policy: on-request",
        "Approval policy: never",
        "Sandbox: read-only",
        "Sandbox: workspace-write",
        "Sandbox: danger-full-access",
        "Follow Server Settings (reset)",
    ] {
        assert!(picker.contains(choice), "missing {choice}: {picker}");
    }
    assert!(
        picker.contains("draft survives posture changes"),
        "{picker}"
    );

    for _ in 0..5 {
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ApprovalPostureNext,
            )))
            .unwrap();
    }
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::ApprovalPostureSelect,
            )))
            .unwrap(),
        ApplicationTransition::UpdateApprovalPosture {
            session: suru::protocol::SessionReference::new(
                suru::protocol::Outlook::Local,
                session_id
            ),
            request: suru::protocol::UpdateApprovalPostureRequest { posture: None },
        }
    );
    let frame = rendered_application_rows_at(&application, 80, 24).join("\n");
    assert!(frame.contains("draft survives posture changes"), "{frame}");
}
