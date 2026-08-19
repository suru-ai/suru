use std::path::PathBuf;

use chidori::protocol::{
    Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity,
    CreateSessionRequest, FileChange, InitialPrompt, Message, MessageId, MessageRole,
    MessageStatus, ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice,
    ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole,
    ModelOptionValue, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderId,
    Session, SessionChange, SessionError, SessionErrorCode, SessionId, SessionRevision,
    SessionSnapshot, SessionStatus, SessionSummary, SessionTimestamp, SessionUpdate,
    TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
};
use serde_json::json;
use uuid::Uuid;

fn fixture_id(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("parse fixture identity")
}

#[test]
fn session_summary_round_trips_with_discovery_metadata() {
    let summary = SessionSummary {
        session: Session {
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            workspace: Workspace {
                path: PathBuf::from("/work/chidori"),
            },
            agent: Some(AgentIdentity {
                agent: AgentId::new("coding"),
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5"),
            }),
            status: SessionStatus::Active,
        },
        title: "Explain this workspace".to_owned(),
        created_at: SessionTimestamp(1_755_497_600_000),
        updated_at: SessionTimestamp(1_755_497_600_321),
    };
    let expected = json!({
        "id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "title": "Explain this workspace",
        "workspace": { "path": "/work/chidori" },
        "agent": {
            "agent": "coding",
            "provider": "codex",
            "model": "gpt-5"
        },
        "status": "active",
        "created_at": 1_755_497_600_000_u64,
        "updated_at": 1_755_497_600_321_u64
    });

    assert_eq!(
        serde_json::to_value(&summary).expect("encode Session summary"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionSummary>(expected).expect("decode Session summary"),
        summary
    );
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
                agent: AgentId::new("coding"),
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5"),
            }),
            status: SessionStatus::Idle,
        },
        revision: SessionRevision(7),
        prompts: vec![Prompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(1),
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
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
        }],
        activities: vec![Activity::Error {
            id: ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87")),
            turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            text: "No Agent is selected".to_owned(),
        }],
        transcript: vec![
            TranscriptItem::Message {
                message_id: MessageId::from_uuid(fixture_id(
                    "0198b27e-310d-763a-9825-51cc8b2bef81",
                )),
            },
            TranscriptItem::Activity {
                activity_id: ActivityId::from_uuid(fixture_id(
                    "0198b27e-345a-700e-ae3b-d971c57fbe87",
                )),
            },
        ],
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
            "delivery": "steer",
            "admission_order": 1,
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
            "status": "completed",
            "content": "Explain this workspace"
        }],
        "activities": [{
            "id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "kind": "error",
            "text": "No Agent is selected"
        }],
        "transcript": [
            {
                "type": "message",
                "message_id": "0198b27e-310d-763a-9825-51cc8b2bef81"
            },
            {
                "type": "activity",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87"
            }
        ]
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
fn agent_message_streaming_uses_one_stable_provider_neutral_identity() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let message_id = MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::MessageAdded {
                message: Message {
                    id: message_id,
                    turn_id,
                    role: MessageRole::Agent,
                    status: MessageStatus::Streaming,
                    content: String::new(),
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::MessageContentAppended {
                message_id,
                content: "Hello".to_owned(),
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![SessionChange::MessageCompleted { message_id }],
        },
    ];
    let expected = json!([
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 8,
            "changes": [{
                "type": "message_added",
                "message": {
                    "id": "0198b27e-310d-763a-9825-51cc8b2bef81",
                    "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                    "role": "agent",
                    "status": "streaming",
                    "content": ""
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "message_content_appended",
                "message_id": "0198b27e-310d-763a-9825-51cc8b2bef81",
                "content": "Hello"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [{
                "type": "message_completed",
                "message_id": "0198b27e-310d-763a-9825-51cc8b2bef81"
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode Agent Message updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 3]>(expected)
            .expect("decode Agent Message updates"),
        updates
    );
}

#[test]
fn command_activity_lifecycle_uses_typed_incremental_updates() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::Command {
                    id: activity_id,
                    turn_id,
                    status: ActivityStatus::Active,
                    command: "cargo test --test session_protocol".to_owned(),
                    cwd: Some(PathBuf::from("/work/chidori")),
                    output: String::new(),
                    exit_status: None,
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::CommandOutputAppended {
                activity_id,
                content: "running 1 test\n".to_owned(),
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![SessionChange::CommandStatusChanged {
                activity_id,
                status: ActivityStatus::Completed,
                exit_status: Some(0),
            }],
        },
    ];
    let expected = json!([
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 8,
            "changes": [{
                "type": "activity_added",
                "activity": {
                    "id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                    "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                    "kind": "command",
                    "status": "active",
                    "command": "cargo test --test session_protocol",
                    "cwd": "/work/chidori",
                    "output": "",
                    "exit_status": null
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "command_output_appended",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "content": "running 1 test\n"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [{
                "type": "command_status_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "completed",
                "exit_status": 0
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode command Activity updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 3]>(expected)
            .expect("decode command Activity updates"),
        updates
    );
}

#[test]
fn file_change_activity_lifecycle_uses_typed_incremental_updates() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::FileChange {
                    id: activity_id,
                    turn_id,
                    status: ActivityStatus::Active,
                    changes: vec![FileChange::Update {
                        path: PathBuf::from("src/protocol.rs"),
                        moved_to: Some(PathBuf::from("src/protocol_v2.rs")),
                    }],
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::FileChangeUpdated {
                activity_id,
                changes: vec![
                    FileChange::Update {
                        path: PathBuf::from("src/protocol.rs"),
                        moved_to: Some(PathBuf::from("src/protocol_v2.rs")),
                    },
                    FileChange::Add {
                        path: PathBuf::from("tests/session_protocol.rs"),
                    },
                ],
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![SessionChange::FileChangeStatusChanged {
                activity_id,
                status: ActivityStatus::Completed,
            }],
        },
    ];
    let expected = json!([
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 8,
            "changes": [{
                "type": "activity_added",
                "activity": {
                    "id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                    "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                    "kind": "file_change",
                    "status": "active",
                    "changes": [{
                        "path": "src/protocol.rs",
                        "kind": "update",
                        "moved_to": "src/protocol_v2.rs"
                    }]
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "file_change_updated",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "changes": [
                    {
                        "path": "src/protocol.rs",
                        "kind": "update",
                        "moved_to": "src/protocol_v2.rs"
                    },
                    {
                        "path": "tests/session_protocol.rs",
                        "kind": "add"
                    }
                ]
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [{
                "type": "file_change_status_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "completed"
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode file-change Activity updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 3]>(expected)
            .expect("decode file-change Activity updates"),
        updates
    );
}

#[test]
fn file_change_updates_reject_opaque_public_protocol_fields() {
    let update = json!({
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "revision": 10,
        "changes": [{
            "type": "file_change_status_changed",
            "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
            "status": "completed",
            "provider_payload": { "opaque": true }
        }]
    });

    assert!(
        serde_json::from_value::<SessionUpdate>(update).is_err(),
        "public Session changes must reject arbitrary Provider fields"
    );
}

#[test]
fn agent_binding_is_a_typed_session_change() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(2),
        changes: vec![SessionChange::AgentBound {
            agent: AgentIdentity {
                agent: AgentId::new("codex"),
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5.6-codex"),
            },
        }],
    };
    let expected = json!({
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "revision": 2,
        "changes": [{
            "type": "agent_bound",
            "agent": {
                "agent": "codex",
                "provider": "codex",
                "model": "gpt-5.6-codex"
            }
        }]
    });

    assert_eq!(
        serde_json::to_value(&update).expect("encode Agent-binding update"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionUpdate>(expected).expect("decode Agent-binding update"),
        update
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
fn prompt_admission_command_round_trips_with_its_client_generated_identity() {
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Steer the current Session".to_owned(),
        },
        delivery: PromptDelivery::Queue,
    };
    let expected = json!({
        "prompt": {
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Steer the current Session"
        },
        "delivery": "queue"
    });

    assert_eq!(
        serde_json::to_value(&command).expect("encode command"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<AdmitPromptRequest>(expected).expect("decode command"),
        command
    );
}

#[test]
fn session_delta_status_and_error_contracts_use_stable_provider_neutral_shapes() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let prompt_id = PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(8),
        changes: vec![
            SessionChange::PromptDeliveryChanged {
                prompt_id,
                delivery: PromptDelivery::Steer,
            },
            SessionChange::PromptStatusChanged {
                prompt_id,
                status: PromptStatus::Delivered,
            },
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
                    "type": "prompt_delivery_changed",
                    "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
                    "delivery": "steer"
                },
                {
                    "type": "prompt_status_changed",
                    "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
                    "status": "delivered"
                },
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
        serde_json::to_value(SessionErrorCode::TurnInterruptionFailed)
            .expect("encode interruption failure code"),
        json!("turn_interruption_failed")
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

#[test]
fn model_descriptors_and_option_values_use_strict_typed_json() {
    let descriptor = ModelDescriptor {
        provider: ProviderId::new("codex"),
        id: ModelId::new("provider/model:opaque"),
        display_name: "Model Name".to_owned(),
        description: "Model description".to_owned(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: vec![
            ModelOptionDescriptor {
                id: ModelOptionId::new("effort-native"),
                label: "Reasoning".to_owned(),
                description: Some("Controls thinking".to_owned()),
                role: ModelOptionRole::ReasoningEffort,
                kind: ModelOptionKind::Select {
                    choices: vec![ModelOptionChoice {
                        id: ModelOptionChoiceId::new("provider-high"),
                        label: "High".to_owned(),
                        description: None,
                        availability: ModelAvailability::Available,
                    }],
                    default: ModelOptionChoiceId::new("provider-high"),
                },
            },
            ModelOptionDescriptor {
                id: ModelOptionId::new("preview"),
                label: "Preview".to_owned(),
                description: None,
                role: ModelOptionRole::Other,
                kind: ModelOptionKind::Toggle { default: false },
            },
        ],
    };
    let encoded = serde_json::to_value(&descriptor).expect("encode Model descriptor");
    assert_eq!(
        encoded,
        json!({
            "provider": "codex",
            "id": "provider/model:opaque",
            "display_name": "Model Name",
            "description": "Model description",
            "is_default": true,
            "availability": "available",
            "options": [
                {
                    "id": "effort-native",
                    "label": "Reasoning",
                    "description": "Controls thinking",
                    "role": "reasoning_effort",
                    "kind": {
                        "type": "select",
                        "choices": [{
                            "id": "provider-high",
                            "label": "High",
                            "description": null,
                            "availability": "available"
                        }],
                        "default": "provider-high"
                    }
                },
                {
                    "id": "preview",
                    "label": "Preview",
                    "description": null,
                    "role": "other",
                    "kind": { "type": "toggle", "default": false }
                }
            ]
        })
    );
    assert_eq!(
        serde_json::from_value::<ModelDescriptor>(encoded).expect("decode Model descriptor"),
        descriptor
    );
    assert_eq!(
        serde_json::to_value([
            ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("provider-high")
            },
            ModelOptionValue::Toggle { enabled: true },
        ])
        .expect("encode Model Option values"),
        json!([
            { "type": "select", "choice": "provider-high" },
            { "type": "toggle", "enabled": true }
        ])
    );
    assert!(
        serde_json::from_value::<ModelOptionValue>(
            json!({ "type": "toggle", "enabled": true, "provider_data": 1 })
        )
        .is_err()
    );
}
