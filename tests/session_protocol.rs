use std::path::PathBuf;

use serde_json::json;
use suru::protocol::{
    Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity,
    AgentSelection, AgentSelectionOperationId, CreateSessionRequest, FileChange, InitialPrompt,
    Message, MessageId, MessageRole, MessageStatus, ModelAvailability, ModelDescriptor, ModelId,
    ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
    ModelOptionRole, ModelOptionSelection, ModelOptionValue, Prompt, PromptDelivery, PromptId,
    PromptOrder, PromptStatus, ProviderId, Session, SessionChange, SessionError, SessionErrorCode,
    SessionId, SessionRevision, SessionSnapshot, SessionStatus, SessionSummary, SessionTimestamp,
    SessionUpdate, SkillCatalog, SkillCatalogCapabilities, SkillCatalogRequest, SkillCatalogStatus,
    SkillDescriptor, SkillId, SkillInvocation, SkillMarkerSpan, SkillPromptDelivery,
    TranscriptItem, Turn, TurnId, TurnStatus, UpdateAgentSelectionRequest, Workspace,
};
use uuid::Uuid;

fn fixture_id(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("parse fixture identity")
}

#[test]
fn workspace_skill_catalog_round_trips_only_safe_provider_neutral_metadata() {
    let catalog = SkillCatalog {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: PathBuf::from("/work/suru"),
        },
        skills: vec![SkillDescriptor {
            id: SkillId::new("01J-safe-opaque-id"),
            name: "code-review".to_owned(),
            description: "Review a change against its specification.".to_owned(),
            scope: Some("Workspace".to_owned()),
        }],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: Some(3),
            supported_deliveries: vec![
                SkillPromptDelivery::Initial,
                SkillPromptDelivery::Queue,
                SkillPromptDelivery::Steer,
            ],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    };
    let expected = json!({
        "provider": "codex",
        "workspace": { "path": "/work/suru" },
        "skills": [{
            "id": "01J-safe-opaque-id",
            "name": "code-review",
            "description": "Review a change against its specification.",
            "scope": "Workspace"
        }],
        "capabilities": {
            "max_distinct_invocations": 3,
            "supported_deliveries": ["initial", "queue", "steer"]
        },
        "status": { "state": "fresh", "warning": null }
    });

    assert_eq!(
        serde_json::to_value(&catalog).expect("encode Skill Catalog"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SkillCatalog>(expected).expect("decode Skill Catalog"),
        catalog
    );
}

#[test]
fn skill_catalog_request_round_trips_provider_and_workspace_context() {
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: PathBuf::from("workspace"),
        },
    };
    let encoded = json!({
        "provider": "codex",
        "workspace": { "path": "workspace" }
    });
    assert_eq!(
        serde_json::to_value(&request).expect("encode request"),
        encoded
    );
    assert_eq!(
        serde_json::from_value::<SkillCatalogRequest>(encoded).expect("decode request"),
        request
    );
}

#[test]
fn safe_skill_invocations_round_trip_beside_the_original_prompt_text() {
    let prompt = InitialPrompt {
        id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
        text: "$code-review check this".to_owned(),
        skill_invocations: vec![SkillInvocation {
            skill_id: SkillId::new("01J-safe-opaque-id"),
            name: "code-review".to_owned(),
            scope: Some("Workspace".to_owned()),
            marker: SkillMarkerSpan { start: 0, end: 12 },
        }],
    };
    let encoded = serde_json::to_value(&prompt).expect("encode Skill-bearing Prompt");

    assert_eq!(
        encoded,
        json!({
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "$code-review check this",
            "skill_invocations": [{
                "skill_id": "01J-safe-opaque-id",
                "name": "code-review",
                "scope": "Workspace",
                "marker": { "start": 0, "end": 12 }
            }]
        })
    );
    assert_eq!(
        serde_json::from_value::<InitialPrompt>(encoded).expect("decode Skill-bearing Prompt"),
        prompt
    );
}

#[test]
fn agent_selection_round_trips_complete_typed_model_options() {
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5.6-codex"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("fast_mode"),
                value: ModelOptionValue::Toggle { enabled: true },
            },
        ],
    };
    let expected = json!({
        "provider": "codex",
        "model": "gpt-5.6-codex",
        "options": [
            {
                "id": "reasoning_effort",
                "value": { "type": "select", "choice": "high" }
            },
            {
                "id": "fast_mode",
                "value": { "type": "toggle", "enabled": true }
            }
        ]
    });

    assert_eq!(
        serde_json::to_value(&selection).expect("encode Agent Selection"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<AgentSelection>(expected).expect("decode Agent Selection"),
        selection
    );
    assert!(
        serde_json::from_value::<AgentSelection>(json!({
            "provider": "codex",
            "model": "gpt-5.6-codex",
            "options": [{ "id": "reasoning_effort", "value": 3 }]
        }))
        .is_err(),
        "Model Option values reject arbitrary JSON"
    );
}

#[test]
fn session_summary_round_trips_with_discovery_metadata() {
    let summary = SessionSummary {
        session: Session {
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            workspace: Workspace {
                path: PathBuf::from("/work/suru"),
            },
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5"),
                options: Vec::new(),
            }),
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Active,
        },
        title: "Explain this workspace".to_owned(),
        emoji: Some("\u{1F5FA}\u{FE0F}".to_owned()),
        created_at: SessionTimestamp(1_755_497_600_000),
        updated_at: SessionTimestamp(1_755_497_600_321),
    };
    let expected = json!({
        "id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "title": "Explain this workspace",
        "emoji": "\u{1F5FA}\u{FE0F}",
        "workspace": { "path": "/work/suru" },
        "agent_selection": {
            "provider": "codex",
            "model": "gpt-5",
            "options": []
        },
        "agent_selection_availability": "available",
        "status": "active",
        "created_at": 1_755_497_600_000_u64,
        "updated_at": 1_755_497_600_321_u64
    });

    assert_eq!(
        serde_json::to_value(&summary).expect("encode Session summary"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionSummary>(expected.clone()).expect("decode Session summary"),
        summary
    );

    // A Session whose Title was never derived carries no Emoji at all, which is
    // every Session that predates Title derivation.
    let mut without_emoji = expected;
    without_emoji
        .as_object_mut()
        .expect("the encoded summary is an object")
        .remove("emoji");
    assert_eq!(
        serde_json::from_value::<SessionSummary>(without_emoji)
            .expect("decode a Session summary carrying no Emoji")
            .emoji,
        None
    );
}

#[test]
fn provider_neutral_session_snapshot_round_trips_through_json() {
    let snapshot = SessionSnapshot {
        session: Session {
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            workspace: Workspace {
                path: PathBuf::from("/work/suru"),
            },
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5"),
                options: vec![ModelOptionSelection {
                    id: ModelOptionId::new("reasoning_effort"),
                    value: ModelOptionValue::Select {
                        choice: ModelOptionChoiceId::new("high"),
                    },
                }],
            }),
            agent_selection_availability: ModelAvailability::Unavailable,
            status: SessionStatus::Idle,
        },
        revision: SessionRevision(7),
        prompts: vec![Prompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(1),
            status: PromptStatus::Delivered,
        }],
        turns: vec![Turn {
            id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            prompt_id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            agent: Some(AgentIdentity {
                agent: AgentId::new("coding"),
                selection: AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-5"),
                    options: vec![ModelOptionSelection {
                        id: ModelOptionId::new("reasoning_effort"),
                        value: ModelOptionValue::Select {
                            choice: ModelOptionChoiceId::new("high"),
                        },
                    }],
                },
            }),
            status: TurnStatus::Failed,
            started_at: Some(SessionTimestamp(1_755_000_000_000)),
            settled_at: Some(SessionTimestamp(1_755_000_004_200)),
        }],
        messages: vec![Message {
            id: MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81")),
            turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            truncated: false,
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
            "workspace": { "path": "/work/suru" },
            "agent_selection": {
                "provider": "codex",
                "model": "gpt-5",
                "options": [{
                    "id": "reasoning_effort",
                    "value": { "type": "select", "choice": "high" }
                }]
            },
            "agent_selection_availability": "unavailable",
            "status": "idle"
        },
        "revision": 7,
        "prompts": [{
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Explain this workspace",
            "skill_invocations": [],
            "delivery": "steer",
            "admission_order": 1,
            "status": "delivered"
        }],
        "turns": [{
            "id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "agent": {
                "agent": "coding",
                "selection": {
                    "provider": "codex",
                    "model": "gpt-5",
                    "options": [{
                        "id": "reasoning_effort",
                        "value": { "type": "select", "choice": "high" }
                    }]
                }
            },
            "status": "failed",
            "started_at": 1_755_000_000_000_u64,
            "settled_at": 1_755_000_004_200_u64
        }],
        "messages": [{
            "id": "0198b27e-310d-763a-9825-51cc8b2bef81",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
            "role": "user",
            "status": "completed",
            "content": "Explain this workspace",
            "skill_invocations": [],
            "truncated": false
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
                    skill_invocations: Vec::new(),
                    truncated: false,
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
            changes: vec![SessionChange::MessageTruncated { message_id }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(11),
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
                    "content": "",
                    "skill_invocations": [],
                    "truncated": false
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
                "type": "message_truncated",
                "message_id": "0198b27e-310d-763a-9825-51cc8b2bef81"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 11,
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
        serde_json::from_value::<[SessionUpdate; 4]>(expected)
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
                    cwd: Some(PathBuf::from("/work/suru")),
                    output: String::new(),
                    output_truncated: false,
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
            changes: vec![SessionChange::CommandOutputTruncated { activity_id }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(11),
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
                    "cwd": "/work/suru",
                    "output": "",
                    "output_truncated": false,
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
                "type": "command_output_truncated",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 11,
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
        serde_json::from_value::<[SessionUpdate; 4]>(expected)
            .expect("decode command Activity updates"),
        updates
    );
}

#[test]
fn reasoning_activity_lifecycle_uses_typed_incremental_updates() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::Reasoning {
                    id: activity_id,
                    turn_id,
                    status: ActivityStatus::Active,
                    title: None,
                    content: String::new(),
                    content_truncated: false,
                    duration_ms: None,
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::ReasoningTitleChanged {
                activity_id,
                title: "Inspecting the seam".to_owned(),
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![SessionChange::ReasoningContentAppended {
                activity_id,
                content: "Reading the projection.".to_owned(),
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(11),
            changes: vec![SessionChange::ReasoningContentTruncated { activity_id }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(12),
            changes: vec![SessionChange::ReasoningStatusChanged {
                activity_id,
                status: ActivityStatus::Completed,
                duration_ms: Some(72_000),
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
                    "kind": "reasoning",
                    "status": "active",
                    "title": null,
                    "content": "",
                    "content_truncated": false,
                    "duration_ms": null
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "reasoning_title_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "title": "Inspecting the seam"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [{
                "type": "reasoning_content_appended",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "content": "Reading the projection."
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 11,
            "changes": [{
                "type": "reasoning_content_truncated",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 12,
            "changes": [{
                "type": "reasoning_status_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "completed",
                "duration_ms": 72000
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode Reasoning Activity updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 5]>(expected)
            .expect("decode Reasoning Activity updates"),
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
fn agent_selection_change_is_typed_and_replaceable() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(2),
        changes: vec![SessionChange::AgentSelectionChanged {
            selection: AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5.6-codex"),
                options: Vec::new(),
            },
        }],
    };
    let expected = json!({
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "revision": 2,
        "changes": [{
            "type": "agent_selection_changed",
            "selection": {
                "provider": "codex",
                "model": "gpt-5.6-codex",
                "options": []
            }
        }]
    });

    assert_eq!(
        serde_json::to_value(&update).expect("encode Agent Selection update"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionUpdate>(expected).expect("decode Agent Selection update"),
        update
    );
    assert!(
        serde_json::from_value::<SessionUpdate>(json!({
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
        }))
        .is_err(),
        "the removed one-time Agent binding is not a compatibility path"
    );
}

#[test]
fn initial_session_command_round_trips_through_json() {
    let command = CreateSessionRequest {
        agent_selection: None,
        workspace: Workspace {
            path: PathBuf::from("/work/suru"),
        },
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
        },
    };
    let expected = json!({
        "agent_selection": null,
        "workspace": { "path": "/work/suru" },
        "prompt": {
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Explain this workspace",
            "skill_invocations": []
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
fn agent_selection_command_round_trips_with_its_operation_identity() {
    let command = UpdateAgentSelectionRequest {
        operation_id: AgentSelectionOperationId::from_uuid(fixture_id(
            "0198b27e-3aa1-72dd-9ec8-65398d17ec16",
        )),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5.6-codex"),
            options: Vec::new(),
        },
    };
    let expected = json!({
        "operation_id": "0198b27e-3aa1-72dd-9ec8-65398d17ec16",
        "selection": {
            "provider": "codex",
            "model": "gpt-5.6-codex",
            "options": []
        }
    });

    assert_eq!(
        serde_json::to_value(&command).expect("encode Agent Selection command"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<UpdateAgentSelectionRequest>(expected)
            .expect("decode Agent Selection command"),
        command
    );
    assert!(
        serde_json::from_value::<UpdateAgentSelectionRequest>(json!({
            "operation_id": "0198b27e-3aa1-72dd-9ec8-65398d17ec16",
            "selection": {
                "provider": "codex",
                "model": "gpt-5.6-codex",
                "options": []
            },
            "unexpected": true
        }))
        .is_err(),
        "Agent Selection commands reject unknown fields"
    );
}

#[test]
fn prompt_admission_command_round_trips_with_its_client_generated_identity() {
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Steer the current Session".to_owned(),
            skill_invocations: Vec::new(),
        },
        delivery: PromptDelivery::Queue,
    };
    let expected = json!({
        "prompt": {
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Steer the current Session",
            "skill_invocations": []
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
                settled_at: Some(SessionTimestamp(1_755_000_004_200)),
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
                    "status": "completed",
                    "settled_at": 1_755_000_004_200_u64
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

#[test]
fn model_descriptors_materialize_complete_defaults_and_preserve_valid_same_model_options() {
    let descriptor = ModelDescriptor {
        provider: ProviderId::new("provider-opaque"),
        id: ModelId::new("model-opaque"),
        display_name: "Opaque Model".to_owned(),
        description: "Advertises ordered independent options".to_owned(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: vec![
            ModelOptionDescriptor {
                id: ModelOptionId::new("reasoning-opaque"),
                label: "Reasoning".to_owned(),
                description: None,
                role: ModelOptionRole::ReasoningEffort,
                kind: ModelOptionKind::Select {
                    choices: vec![
                        ModelOptionChoice {
                            id: ModelOptionChoiceId::new("low-opaque"),
                            label: "Low".to_owned(),
                            description: None,
                            availability: ModelAvailability::Available,
                        },
                        ModelOptionChoice {
                            id: ModelOptionChoiceId::new("high-opaque"),
                            label: "High".to_owned(),
                            description: None,
                            availability: ModelAvailability::Available,
                        },
                    ],
                    default: ModelOptionChoiceId::new("high-opaque"),
                },
            },
            ModelOptionDescriptor {
                id: ModelOptionId::new("speed-opaque"),
                label: "Fast".to_owned(),
                description: None,
                role: ModelOptionRole::Speed,
                kind: ModelOptionKind::Toggle { default: false },
            },
        ],
    };
    let defaults = descriptor.default_agent_selection();
    assert_eq!(
        defaults.options,
        vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high-opaque"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Toggle { enabled: false },
            },
        ]
    );

    let other_model = AgentSelection {
        provider: descriptor.provider.clone(),
        model: ModelId::new("other-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low-opaque"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Toggle { enabled: true },
            },
        ],
    };
    assert_eq!(
        descriptor
            .materialize_agent_selection(Some(&other_model))
            .expect("switching Models materializes defaults"),
        defaults,
        "switching Models uses the target Model's defaults"
    );

    let same_model = AgentSelection {
        provider: descriptor.provider.clone(),
        model: descriptor.id.clone(),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low-opaque"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Toggle { enabled: true },
            },
        ],
    };
    assert_eq!(
        descriptor
            .materialize_agent_selection(Some(&same_model))
            .expect("same-Model options are valid"),
        same_model,
        "reopening the same Model preserves its complete current options"
    );

    let incomplete = AgentSelection {
        options: same_model.options[..1].to_vec(),
        ..same_model.clone()
    };
    assert!(
        descriptor
            .materialize_agent_selection(Some(&incomplete))
            .is_err(),
        "same-Model options are never silently replaced with defaults"
    );
    let unavailable = AgentSelection {
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("retired-opaque"),
                },
            },
            same_model.options[1].clone(),
        ],
        ..same_model
    };
    assert!(
        descriptor
            .materialize_agent_selection(Some(&unavailable))
            .is_err(),
        "unavailable same-Model choices remain an explicit error"
    );
}
