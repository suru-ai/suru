use std::path::PathBuf;

use chidori::protocol::{
    Activity, ActivityId, ActivityKind, AgentIdentity, CreateSessionRequest, InitialPrompt,
    Message, MessageId, MessageRole, Prompt, PromptId, PromptStatus, Session, SessionChange,
    SessionError, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
    SessionUpdate, Turn, TurnId, TurnStatus, Workspace,
};
use serde_json::json;
use uuid::Uuid;

fn fixture_id(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("parse fixture identity")
}

#[test]
fn provider_neutral_session_snapshot_round_trips_through_json() {
    let snapshot = SessionSnapshot {
        session: Session {
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            workspace: Workspace {
                path: PathBuf::from("/work/chidori"),
            },
            agent: Some(AgentIdentity {
                agent: "coding".to_owned(),
                provider: "codex".to_owned(),
                model: "gpt-5".to_owned(),
            }),
            status: SessionStatus::Idle,
        },
        revision: SessionRevision(7),
        prompts: vec![Prompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            status: PromptStatus::Delivered,
        }],
        turns: vec![Turn {
            id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            prompt_id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            status: TurnStatus::Failed,
        }],
        messages: vec![Message {
            id: MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81")),
            turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            role: MessageRole::User,
            content: "Explain this workspace".to_owned(),
        }],
        activities: vec![Activity {
            id: ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87")),
            turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            kind: ActivityKind::Error,
            text: "No Agent is selected".to_owned(),
        }],
    };
    let expected = json!({
        "session": {
            "id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "workspace": { "path": "/work/chidori" },
            "agent": {
                "agent": "coding",
                "provider": "codex",
                "model": "gpt-5"
            },
            "status": "idle"
        },
        "revision": 7,
        "prompts": [{
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Explain this workspace",
            "status": "delivered"
        }],
        "turns": [{
            "id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "status": "failed"
        }],
        "messages": [{
            "id": "0198b27e-310d-763a-9825-51cc8b2bef81",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "role": "user",
            "content": "Explain this workspace"
        }],
        "activities": [{
            "id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "kind": "error",
            "text": "No Agent is selected"
        }]
    });

    assert_eq!(
        serde_json::to_value(&snapshot).expect("encode snapshot"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionSnapshot>(expected).expect("decode snapshot"),
        snapshot
    );
}

#[test]
fn initial_session_command_round_trips_through_json() {
    let command = CreateSessionRequest {
        workspace: Workspace {
            path: PathBuf::from("/work/chidori"),
        },
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
        },
    };
    let expected = json!({
        "workspace": { "path": "/work/chidori" },
        "prompt": {
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Explain this workspace"
        }
    });

    assert_eq!(
        serde_json::to_value(&command).expect("encode command"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<CreateSessionRequest>(expected).expect("decode command"),
        command
    );
}

#[test]
fn session_delta_status_and_error_contracts_use_stable_provider_neutral_shapes() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(8),
        changes: vec![
            SessionChange::TurnStatusChanged {
                turn_id,
                status: TurnStatus::Completed,
            },
            SessionChange::SessionStatusChanged {
                status: SessionStatus::Idle,
            },
        ],
    };
    assert_eq!(
        serde_json::to_value(update).expect("encode Session update"),
        json!({
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 8,
            "changes": [
                {
                    "type": "turn_status_changed",
                    "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                    "status": "completed"
                },
                {
                    "type": "session_status_changed",
                    "status": "idle"
                }
            ]
        })
    );
    assert_eq!(
        serde_json::to_value(SessionError {
            code: SessionErrorCode::InvalidWorkspace,
            message: "Workspace must be an existing local directory".to_owned(),
        })
        .expect("encode Session error"),
        json!({
            "code": "invalid_workspace",
            "message": "Workspace must be an existing local directory"
        })
    );
    assert_eq!(
        serde_json::to_value([
            PromptStatus::Pending,
            PromptStatus::Delivered,
            PromptStatus::Cancelled,
        ])
        .expect("encode Prompt statuses"),
        json!(["pending", "delivered", "cancelled"])
    );
    assert_eq!(
        serde_json::to_value([
            TurnStatus::Active,
            TurnStatus::Completed,
            TurnStatus::Failed,
            TurnStatus::Interrupted,
        ])
        .expect("encode Turn statuses"),
        json!(["active", "completed", "failed", "interrupted"])
    );
}
