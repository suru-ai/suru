use std::path::PathBuf;

use serde_json::json;
use suru::protocol::{
    Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity,
    AgentSelection, AgentSelectionOperationId, Answer, ApprovalSubject, AttachmentBinding,
    AttachmentDescriptor, AttachmentId, AttachmentKind, Author, CompactSessionRequest,
    CompactionTrigger, Cost, CostBasis, CostTotal, CreateSessionRequest, Delegator, FileChange,
    InitialPrompt, Message, MessageId, MessageRole, MessageStatus, ModelAvailability,
    ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor,
    ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue,
    PROTOCOL_VERSION, PreparationId, PreparationPrompt, PrepareCheckoutRequest, Prompt,
    PromptDelivery, PromptId, PromptOrder, PromptStatus, PromptWithdrawal, ProviderId, Question,
    QuestionAnswer, Questionnaire, QuestionnaireId, QuestionnaireOutcome, Session, SessionChange,
    SessionError, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
    SessionSummary, SessionTimestamp, SessionUpdate, SkillCatalog, SkillCatalogCapabilities,
    SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor, SkillId, SkillInvocation,
    SkillPromptDelivery, TextSpan, TranscriptItem, Turn, TurnId, TurnStatus,
    UpdateAgentSelectionRequest, Usage, UsageTotal, ViewSessionOperationId, ViewSessionRequest,
    Workspace,
};
use uuid::Uuid;

fn fixture_id(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("parse fixture identity")
}

#[test]
fn approval_subjects_have_strict_provider_neutral_wire_shapes() {
    let subjects = [
        json!({
            "kind": "command",
            "command": "cat Cargo.toml",
            "cwd": "workspace",
            "actions": [{
                "kind": "read",
                "command": "cat Cargo.toml",
                "name": "Cargo.toml",
                "path": "workspace/Cargo.toml"
            }]
        }),
        json!({
            "kind": "file_change",
            "paths": ["src/lib.rs", "src/main.rs"],
            "grant_root": "src"
        }),
        json!({"kind": "read", "path": "secrets.txt"}),
        json!({"kind": "network", "host_or_url": "api.example.test"}),
        json!({
            "kind": "permission_grant",
            "profile": {"sandbox": "workspace-write", "network": false}
        }),
        json!({
            "kind": "other_tool",
            "name": "database_query",
            "input": {"table": "sessions", "limit": 10}
        }),
    ];

    for expected in subjects {
        let subject = serde_json::from_value::<ApprovalSubject>(expected.clone())
            .expect("decode typed Approval subject");
        assert_eq!(
            serde_json::to_value(subject).expect("encode typed Approval subject"),
            expected
        );
    }
    assert!(
        serde_json::from_value::<ApprovalSubject>(
            json!({"kind": "network", "host_or_url": "example.test", "opaque": true})
        )
        .is_err(),
        "Approval subjects reject untyped extension fields"
    );
}

#[test]
fn a_reported_zero_cost_is_known_while_an_unattributed_cost_is_rejected() {
    let zero = Cost::from_usd(0.0).expect("a Provider may report a free Turn");
    assert!(zero.is_zero());
    assert_eq!(
        serde_json::to_value(zero).expect("encode zero Cost"),
        json!(0.0)
    );
    assert_eq!(
        serde_json::from_value::<Cost>(json!(0.0)).expect("decode zero Cost"),
        zero
    );

    let invalid_turn = json!({
        "id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
        "prompt_id": null,
        "agent": null,
        "status": "completed",
        "started_at": null,
        "settled_at": null,
        "usage": null,
        "cost": 0.0,
        "cost_basis": null
    });
    assert!(
        serde_json::from_value::<Turn>(invalid_turn).is_err(),
        "Cost and Cost Basis enter the protocol as one invariant"
    );
}

#[test]
fn workspace_skill_catalog_round_trips_only_safe_provider_neutral_metadata() {
    let catalog = SkillCatalog {
        provider: ProviderId::new("codex"),
        execution_directory: suru::protocol::ExecutionDirectory {
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
        "execution_directory": { "path": "/work/suru" },
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

/// Whether Subsessions are hidden is a Setting every Client is told and may
/// pin: it rides the settings snapshot, which refuses fields it does not
/// know, and has a mutation of its own.
#[test]
fn hiding_subsessions_rides_the_settings_snapshot_and_has_its_own_mutation() {
    const {
        assert!(
            PROTOCOL_VERSION >= 78,
            "a Setting the settings snapshot and its mutations carry changes the wire"
        );
    }
    let mut settings = suru::protocol::EffectiveSettings::default();
    settings.sidekick.hide_subsessions = true;
    let encoded = serde_json::to_value(&settings).expect("encode the effective Settings");
    assert_eq!(encoded["sidekick"], json!({ "hide_subsessions": true }));
    assert_eq!(
        serde_json::from_value::<suru::protocol::EffectiveSettings>(encoded)
            .expect("decode the effective Settings"),
        settings
    );

    let mutation = suru::protocol::SettingMutation::SidekickHideSubsessions { value: Some(true) };
    let expected = json!({ "setting": "sidekick_hide_subsessions", "value": true });
    assert_eq!(
        serde_json::to_value(&mutation).expect("encode the mutation"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<suru::protocol::SettingMutation>(expected)
            .expect("decode the mutation"),
        mutation
    );
}

/// A Workspace carries its Icon and its Description on the wire, and
/// tolerates their absence: a Session's stored metadata written before either
/// field existed holds `Workspace` objects without them at all, and must
/// still decode rather than leave the whole Session unreadable.
#[test]
fn workspace_round_trips_its_icon_and_description_and_tolerates_their_absence() {
    const {
        assert!(
            PROTOCOL_VERSION >= 72,
            "carrying a Workspace's Description, its change, and its endpoint changes the wire"
        );
    }
    let dressed = Workspace {
        id: suru::protocol::WorkspaceId("directory:/work/suru".to_owned()),
        path: PathBuf::from("/work/suru"),
        repository: None,
        source_control: suru::protocol::SourceControlAvailability::NotDetected,
        icon: Some("dev-rust".to_owned()),
        description: Some(suru::protocol::WorkspaceDescription {
            text: "Where Suru itself is built.".to_owned(),
            set: true,
        }),
    };
    let expected = json!({
        "id": "directory:/work/suru",
        "path": "/work/suru",
        "repository": null,
        "source_control": { "status": "not_detected" },
        "icon": "dev-rust",
        "description": { "text": "Where Suru itself is built.", "set": true },
    });
    assert_eq!(
        serde_json::to_value(&dressed).expect("encode Workspace"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<Workspace>(expected).expect("decode Workspace"),
        dressed
    );

    let undressed = json!({
        "id": "directory:/work/suru",
        "path": "/work/suru",
        "repository": null,
        "source_control": { "status": "not_detected" },
    });
    let decoded = serde_json::from_value::<Workspace>(undressed)
        .expect("decode a Workspace with neither field at all");
    assert_eq!(
        decoded.icon, None,
        "a Workspace predating the Icon field decodes with none rather than refusing"
    );
    assert_eq!(
        decoded.description, None,
        "a Workspace predating the Description field decodes with none rather than refusing"
    );
}

#[test]
fn skill_catalog_request_round_trips_provider_and_workspace_context() {
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: PathBuf::from("workspace"),
        },
    };
    let encoded = json!({
        "provider": "codex",
        "execution_directory": { "path": "workspace" }
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
        attachments: Vec::new(),
        skill_invocations: vec![SkillInvocation {
            skill_id: SkillId::new("01J-safe-opaque-id"),
            name: "code-review".to_owned(),
            scope: Some("Workspace".to_owned()),
            span: TextSpan { start: 0, end: 12 },
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
                "span": { "start": 0, "end": 12 }
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
        checkout_state: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: PathBuf::from("/work/suru"),
            },
            workspace: Workspace::directory(PathBuf::from("/work/suru")),
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-5"),
                options: Vec::new(),
            }),
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Active,
            working_since: Some(SessionTimestamp(1_755_497_600_100)),
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        title: "Explain this workspace".to_owned(),
        icon: Some("md-bug".to_owned()),
        settled_at: Some(SessionTimestamp(1_755_497_600_999)),
        standing_inputs: Default::default(),
        total_usage: Some(UsageTotal {
            fresh_input_tokens: Some(1_200),
            cache_read_tokens: Some(300),
            cache_write_tokens: Some(400),
            output_tokens: Some(900),
            reasoning_tokens: None,
            cost: Cost::from_usd(0.03),
            cost_is_partial: false,
        }),
        own_cost: Cost::from_usd(0.02).map(|cost| CostTotal {
            cost,
            is_partial: false,
        }),
        remote_subsessions: Vec::new(),
        created_at: SessionTimestamp(1_755_497_600_000),
        updated_at: SessionTimestamp(1_755_497_600_321),
    };
    let expected = json!({
        "checkout_state": null,
        "id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "title": "Explain this workspace",
        "icon": "md-bug",
        "settled_at": 1_755_497_600_999_u64,
        "standing_inputs": {
            "subagent_interventions": [],
            "pending_questionnaires": [], "submitting_questionnaires": [],
            "pending_questionnaires_revision": 0,
            "pending_approvals": [], "submitting_approvals": [],
            "pending_approvals_revision": 0,
            "latest_turn": null,
            "viewed_at": null
        },
        "working_since": 1_755_497_600_100_u64,
        "monitoring_since": null,
        "total_usage": {
            "fresh_input_tokens": 1_200,
            "cache_read_tokens": 300,
            "cache_write_tokens": 400,
            "output_tokens": 900,
            "reasoning_tokens": null,
            "cost": 0.03,
            "cost_is_partial": false
        },
        "own_cost": { "cost": 0.02, "is_partial": false },
        "workspace": Workspace::directory(std::path::PathBuf::from("/work/suru")),
        "checkout": null,
        "execution_directory": { "path": "/work/suru" },
        "agent_selection": {
            "provider": "codex",
            "model": "gpt-5",
            "options": []
        },
        "agent_selection_availability": "available",
        "approval_posture": null,
        "status": "active",
        "parent": null,
        "context_fill": null,
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

    // A Session whose Title was never derived carries no Icon at all, which is
    // every Session that predates Title derivation. A Session nobody set aside
    // carries no settle moment either, which is every Session that predates the
    // marker. A Session with nothing running carries no working moment, which
    // is every Session that is not working right now, and one waiting on no
    // Watch carries no Monitoring moment, which is every Session stored at all
    // since Monitoring is never stored. A Session whose Turns
    // reported nothing carries no total, which is every Session stored before
    // Usage was recorded at all.
    let mut without_optionals = expected;
    let fields = without_optionals
        .as_object_mut()
        .expect("the encoded summary is an object");
    fields.remove("icon");
    fields.remove("settled_at");
    fields.remove("working_since");
    fields.remove("monitoring_since");
    fields.remove("total_usage");
    fields.remove("own_cost");
    fields.remove("approval_posture");
    let decoded = serde_json::from_value::<SessionSummary>(without_optionals)
        .expect("decode a Session summary carrying none of them");
    assert_eq!(decoded.icon, None);
    assert_eq!(decoded.settled_at, None);
    assert_eq!(decoded.session.working_since, None);
    assert_eq!(decoded.session.monitoring_since, None);
    assert_eq!(decoded.total_usage, None);
    assert_eq!(decoded.own_cost, None);
}

#[test]
fn provider_neutral_session_snapshot_round_trips_through_json() {
    let snapshot = SessionSnapshot {
        title: String::new(),
        icon: None,
        session: Session {
            checkout: None,
            context_fill: Some(suru::protocol::ContextFill {
                occupied_tokens: 12_400,
                capacity_tokens: Some(200_000),
            }),
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: PathBuf::from("/work/suru"),
            },
            workspace: Workspace::directory(PathBuf::from("/work/suru")),
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
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        revision: SessionRevision(7),
        prompts: vec![Prompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(1),
            status: PromptStatus::Delivered,
            withdrawal: None,
            taken: None,
            author: None,
        }],
        turns: vec![Turn {
            id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            prompt_id: Some(PromptId::from_uuid(fixture_id(
                "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            ))),
            compaction_requested: false,
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
            last_output_at: None,
            usage: Some(Usage {
                fresh_input_tokens: Some(1_200),
                cache_read_tokens: Some(300),
                cache_write_tokens: Some(400),
                output_tokens: Some(900),
                reasoning_tokens: None,
                native_meter: None,
            }),
            cost: Cost::from_usd(0.03),
            cost_basis: Some(CostBasis::Reported),
            cost_details: None,
        }],
        messages: vec![Message {
            id: MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81")),
            turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
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
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: suru::protocol::SessionRevision(0),
        watches: Vec::new(),
        waiting_on_subagents: None,
        subagent_usage: Some(UsageTotal {
            fresh_input_tokens: Some(2_000),
            output_tokens: Some(500),
            cost: Cost::from_usd(0.05),
            ..UsageTotal::default()
        }),
        total_cost: None,
        own_cost: None,
        attachments: Vec::new(),
    };
    let expected = json!({
        "title": "",
        "icon": null,
        "session": {
            "id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "workspace": Workspace::directory(std::path::PathBuf::from("/work/suru")),
        "checkout": null,
            "execution_directory": { "path": "/work/suru" },
            "agent_selection": {
                "provider": "codex",
                "model": "gpt-5",
                "options": [{
                    "id": "reasoning_effort",
                    "value": { "type": "select", "choice": "high" }
                }]
            },
            "agent_selection_availability": "unavailable",
            "approval_posture": null,
            "status": "idle",
            "working_since": null,
            "monitoring_since": null,
            "parent": null,
            "context_fill": { "occupied_tokens": 12_400, "capacity_tokens": 200_000 }
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
            "compaction_requested": false,
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
            "settled_at": 1_755_000_004_200_u64,
            "last_output_at": null,
            "usage": {
                "fresh_input_tokens": 1_200,
                "cache_read_tokens": 300,
                "cache_write_tokens": 400,
                "output_tokens": 900,
                "reasoning_tokens": null,
                "native_meter": null
            },
            "cost": 0.03,
            "cost_basis": "reported",
            "cost_details": null
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
        ],
        "subagent_interventions": [],
        "pending_approvals": [],
        "submitting_approvals": [],
        "pending_approvals_revision": 0,
        "subagent_usage": {
            "fresh_input_tokens": 2_000,
            "cache_read_tokens": null,
            "cache_write_tokens": null,
            "output_tokens": 500,
            "reasoning_tokens": null,
            "cost": 0.05,
            "cost_is_partial": false
        },
        "total_cost": null,
        "own_cost": null,
        "watches": [],
        "attachments": [],
        "waiting_on_subagents": null
    });

    assert_eq!(
        serde_json::to_value(&snapshot).expect("encode snapshot"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionSnapshot>(expected.clone()).expect("decode snapshot"),
        snapshot
    );

    // A Session stored before Subagent Usage rolled up carries no roll-up, and
    // decodes with the total its own Turns state.
    let mut without_rollup = expected;
    without_rollup
        .as_object_mut()
        .expect("the encoded snapshot is an object")
        .remove("subagent_usage");
    let decoded = serde_json::from_value::<SessionSnapshot>(without_rollup)
        .expect("decode a snapshot carrying no roll-up");
    assert_eq!(decoded.subagent_usage, None);
    assert_eq!(
        decoded
            .total_usage()
            .and_then(|total| total.blended_tokens()),
        Some(2_100),
        "a Session with nothing delegated totals its own Turns alone"
    );
}

#[test]
fn a_delegation_message_names_its_delegating_agent_apart_from_user_and_agent_messages() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let sibling_id = SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let message_id = MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81"));
    let delegation = |delegator: Delegator| Message {
        id: message_id,
        turn_id,
        role: MessageRole::Delegation(delegator),
        status: MessageStatus::Completed,
        content: "Tighten the second paragraph.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        truncated: false,
        author: None,
    };
    let from_sibling = delegation(Delegator {
        session_id: sibling_id,
        name: Some("Reviewer".to_owned()),
    });
    let expected = json!({
        "id": "0198b27e-310d-763a-9825-51cc8b2bef81",
        "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
        "role": {
            "delegation": {
                "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
                "name": "Reviewer"
            }
        },
        "status": "completed",
        "content": "Tighten the second paragraph.",
        "skill_invocations": [],
        "truncated": false
    });
    assert_eq!(
        serde_json::to_value(&from_sibling).expect("encode a Delegation"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<Message>(expected).expect("decode a Delegation"),
        from_sibling
    );

    let from_top_level = delegation(Delegator {
        session_id,
        name: None,
    });
    let encoded = serde_json::to_value(&from_top_level).expect("encode a top-level Delegation");
    assert_eq!(
        encoded["role"],
        json!({"delegation": {"session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f", "name": null}}),
        "a top-level Session's Agent is named by its Session alone"
    );
    assert_eq!(
        from_top_level
            .role
            .delegator()
            .map(|delegator| delegator.session_id),
        Some(session_id)
    );
    assert_eq!(MessageRole::User.delegator(), None);
    assert_eq!(
        serde_json::to_value(MessageRole::User).expect("encode a user role"),
        json!("user"),
        "user and agent roles keep their plain wire form"
    );
}

#[test]
fn a_prompt_a_sidekick_sent_and_its_message_name_the_sidekick_and_the_users_own_name_no_one() {
    const {
        assert!(
            PROTOCOL_VERSION >= 73,
            "a Prompt's and a Message's author changes the wire, a Remote's included"
        );
    }
    let sidekick = SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f"));
    let author = Author::Sidekick {
        session_id: sidekick,
        title: "Tidy the listing".to_owned(),
    };
    let message = Message {
        id: MessageId::from_uuid(fixture_id("0198b27e-310d-763a-9825-51cc8b2bef81")),
        turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
        role: MessageRole::User,
        status: MessageStatus::Completed,
        content: "Pick this back up.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        truncated: false,
        author: Some(author.clone()),
    };
    let expected = json!({
        "id": "0198b27e-310d-763a-9825-51cc8b2bef81",
        "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
        "role": "user",
        "status": "completed",
        "content": "Pick this back up.",
        "skill_invocations": [],
        "truncated": false,
        "author": {
            "kind": "sidekick",
            "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "title": "Tidy the listing"
        }
    });
    assert_eq!(
        serde_json::to_value(&message).expect("encode a Sidekick's Message"),
        expected,
        "a Sidekick's Message is still a user Message, naming its author beside it"
    );
    assert_eq!(
        serde_json::from_value::<Message>(expected).expect("decode a Sidekick's Message"),
        message
    );

    let prompt = Prompt {
        id: PromptId::from_uuid(fixture_id("0198b27e-1f22-7b0c-9d3e-6f1a2b3c4d5e")),
        text: "Pick this back up.".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        delivery: PromptDelivery::Queue,
        admission_order: PromptOrder(2),
        status: PromptStatus::Pending,
        taken: None,
        author: Some(author),
        withdrawal: None,
    };
    let encoded = serde_json::to_value(&prompt).expect("encode a Sidekick's Prompt");
    assert_eq!(encoded["author"]["kind"], json!("sidekick"));
    assert_eq!(
        serde_json::from_value::<Prompt>(encoded).expect("decode a Sidekick's Prompt"),
        prompt
    );

    let users_own = Prompt {
        author: None,
        ..prompt
    };
    let encoded = serde_json::to_value(&users_own).expect("encode the user's own Prompt");
    assert!(
        encoded.get("author").is_none(),
        "the user's own Prompt carries no author on the wire: {encoded}"
    );
}

/// An idle top-level Session `id`, begun by the user, working at the root of
/// `workspace`.
fn session_at(id: SessionId, workspace: &std::path::Path) -> Session {
    Session {
        checkout: None,
        context_fill: None,
        id,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        workspace: Workspace::directory(workspace.to_owned()),
        agent_selection: None,
        agent_selection_availability: ModelAvailability::Available,
        approval_posture: None,
        status: SessionStatus::Idle,
        working_since: None,
        monitoring_since: None,
        parent: None,
        begun_by: None,
    }
}

#[test]
fn a_sidekicks_tree_carries_the_sessions_it_has_a_hand_in_and_every_other_tree_is_unchanged() {
    const {
        assert!(
            PROTOCOL_VERSION >= 76,
            "the Sessions a Sidekick's tree carries change the wire"
        );
    }
    use suru::protocol::{
        SubagentTreeChange, SubagentTreeRevision, SubagentTreeSession, SubagentTreeSnapshot,
        SubagentTreeTopLevel,
    };
    let sidekick = SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f"));
    let acted_on = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let workspace = std::env::temp_dir().join("auth");
    let entry = SubagentTreeSession {
        subagents_unshown: false,
        unconfirmed: false,
        session_id: acted_on,
        origin: None,
        unanswered: false,
        title: "Fix the flaky login test".to_owned(),
        subsession: true,
        workspace_path: workspace.clone(),
        workspace_icon: Some("cod-shield".to_owned()),
        provider: Some(ProviderId::new("claude")),
        model: Some(ModelId::new("sonnet")),
        status: Some(ActivityStatus::Completed),
        worked_ms: Some(12_000),
        working_since: None,
        monitoring_since: None,
        needs_intervention: false,
        acted_at: SessionTimestamp(1_700),
    };
    let snapshot = SubagentTreeSnapshot {
        revision: SubagentTreeRevision::INITIAL,
        top_level: SubagentTreeTopLevel {
            own_working_since: None,
            status: None,
            worked_ms: None,
            session_id: sidekick,
            title: "Plan the work".to_owned(),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
            sidekick: true,
        },
        subagents: Vec::new(),
        sessions: vec![entry.clone()],
    };
    let encoded = serde_json::to_value(&snapshot).expect("encode a Sidekick's tree");
    assert_eq!(encoded["top_level"]["sidekick"], json!(true));
    assert_eq!(
        encoded["sessions"],
        json!([{
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "title": "Fix the flaky login test",
            "subsession": true,
            "workspace_path": workspace,
            "workspace_icon": "cod-shield",
            "provider": "claude",
            "model": "sonnet",
            "status": "completed",
            "worked_ms": 12000,
            "working_since": null,
            "monitoring_since": null,
            "needs_intervention": false,
            "acted_at": 1700
        }])
    );
    assert_eq!(
        serde_json::from_value::<SubagentTreeSnapshot>(encoded).expect("decode the tree"),
        snapshot
    );

    let ordinary = SubagentTreeSnapshot {
        top_level: SubagentTreeTopLevel {
            sidekick: false,
            ..snapshot.top_level.clone()
        },
        sessions: Vec::new(),
        ..snapshot.clone()
    };
    let encoded = serde_json::to_value(&ordinary).expect("encode an ordinary tree");
    assert!(
        encoded.get("sessions").is_none() && encoded["top_level"].get("sidekick").is_none(),
        "any other tree is said as it always was: {encoded}"
    );

    assert_eq!(
        serde_json::to_value(SubagentTreeChange::SessionLeft {
            session_id: acted_on,
            origin: None,
        })
        .expect("encode a Session leaving"),
        json!({
            "type": "session_left",
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f"
        })
    );
    // A Remote's Session is named by its Remote, in the tree and leaving it,
    // and one whose Remote does not answer keeps what named it and nothing of
    // its work.
    let unanswered = SubagentTreeSession {
        origin: Some("workstation".to_owned()),
        unanswered: true,
        title: "Write the parser".to_owned(),
        subsession: false,
        workspace_icon: None,
        provider: None,
        model: None,
        status: None,
        worked_ms: None,
        ..entry.clone()
    };
    let encoded = serde_json::to_value(&unanswered).expect("encode a Remote's Session");
    assert_eq!(
        (
            &encoded["origin"],
            &encoded["unanswered"],
            &encoded["title"]
        ),
        (
            &json!("workstation"),
            &json!(true),
            &json!("Write the parser")
        )
    );
    assert_eq!(
        serde_json::from_value::<SubagentTreeSession>(encoded).expect("decode it"),
        unanswered
    );
    assert_eq!(
        serde_json::to_value(SubagentTreeChange::SessionLeft {
            session_id: acted_on,
            origin: Some("workstation".to_owned()),
        })
        .expect("encode a Remote's Session leaving"),
        json!({
            "type": "session_left",
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "origin": "workstation"
        })
    );
    let changed = SubagentTreeChange::SessionChanged { entry };
    assert_eq!(
        serde_json::from_value::<SubagentTreeChange>(
            serde_json::to_value(&changed).expect("encode a Session changing")
        )
        .expect("decode a Session changing"),
        changed
    );
}

/// A Sidekick's act on a Remote travels there with its author, and stands
/// there as a Sidekick's on the Peer it came from, by that Peer's name and by
/// nothing else of it; the Peer is listed by the name it gave itself.
#[test]
fn a_sidekick_on_a_peer_is_named_by_the_peer_alone() {
    const {
        assert!(
            PROTOCOL_VERSION >= 79,
            "an act's author travels between Servers, and a Sidekick on a Peer is a new author"
        );
    }
    let on_peer = Author::PeerSidekick {
        peer: "laptop".to_owned(),
        fingerprint: "ab12cd34ef56".to_owned(),
        act: None,
    };
    let encoded = serde_json::to_value(&on_peer).expect("encode a Sidekick on a Peer");
    assert_eq!(
        encoded,
        json!({ "kind": "peer_sidekick", "peer": "laptop", "fingerprint": "ab12cd34ef56" })
    );
    assert_eq!(
        serde_json::from_value::<Author>(encoded).expect("decode a Sidekick on a Peer"),
        on_peer
    );
    assert_eq!(
        on_peer.sidekick_session(),
        None,
        "it names no Session of the Server holding what it sent"
    );
    assert_eq!(suru::protocol::AUTHOR_HEADER, "x-suru-author");
    // Only the Sidekick's own Server knows it began a Session on a Remote, so
    // the Sidekick's Session names those Sessions for every Client.
    let began = suru::protocol::RemoteSession {
        origin: "workstation".to_owned(),
        session_id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
    };
    let changed = suru::protocol::SessionCatalogChange::RemoteSubsessionsChanged {
        session_id: SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        remote_subsessions: vec![began],
    };
    let encoded = serde_json::to_value(&changed).expect("encode the change");
    assert_eq!(
        encoded,
        json!({
            "type": "remote_subsessions_changed",
            "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "remote_subsessions": [{
                "origin": "workstation",
                "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f"
            }]
        })
    );
    assert_eq!(
        serde_json::from_value::<suru::protocol::SessionCatalogChange>(encoded)
            .expect("decode the change"),
        changed
    );
    let peer = suru::protocol::Peer {
        id: "ab12".to_owned(),
        fingerprint: "ab12".to_owned(),
        name: "laptop".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(&peer).expect("encode a Peer"),
        json!({ "id": "ab12", "fingerprint": "ab12", "name": "laptop" })
    );
}

/// Which Turn took a Prompt, and when, travels with the Prompt — what a
/// Peer's Server reads to know whether a Prompt its Sidekick sent set work
/// going — and a Prompt no Turn took says nothing of it.
#[test]
fn a_prompt_says_which_turn_took_it_and_when() {
    const {
        assert!(
            PROTOCOL_VERSION >= 80,
            "which Turn took a Prompt travels between Servers"
        );
    }
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let taking = suru::protocol::PromptTaking {
        turn_id,
        taken_at: Some(SessionTimestamp(42)),
    };
    let change = SessionChange::PromptTaken {
        prompt_id: PromptId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        taking,
    };
    let encoded = serde_json::to_value(&change).expect("encode the taking");
    assert_eq!(
        encoded,
        json!({
            "type": "prompt_taken",
            "prompt_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "taking": { "turn_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f", "taken_at": 42 }
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionChange>(encoded).expect("decode the taking"),
        change
    );
    let untaken = json!({
        "id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
        "text": "Fix the parser.",
        "skill_invocations": [],
        "delivery": "steer",
        "admission_order": 2,
        "status": "delivered"
    });
    let prompt = serde_json::from_value::<suru::protocol::Prompt>(untaken.clone())
        .expect("a Prompt no Turn took decodes");
    assert_eq!(prompt.taken, None);
    assert_eq!(
        serde_json::to_value(&prompt).expect("encode it again"),
        untaken,
        "and says nothing of a taking"
    );
}

/// A Sidekick on a Peer is named, where it acted, by the act the Peer named
/// too, so the Peer can tell its own Sidekick's act from another's; and when
/// a Questionnaire or Approval was asked and a Questionnaire settled travels
/// with it, on the Serving Server's own clock.
#[test]
fn an_act_on_a_peer_names_itself_and_an_intervention_says_when() {
    const {
        assert!(
            PROTOCOL_VERSION >= 80,
            "an act's own name and an Intervention's moments travel between Servers"
        );
    }
    assert_eq!(suru::protocol::ACT_HEADER, "x-suru-act");
    let act = suru::protocol::ActId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let on_peer = Author::PeerSidekick {
        peer: "laptop".to_owned(),
        fingerprint: "ab12".to_owned(),
        act: Some(act),
    };
    let encoded = serde_json::to_value(&on_peer).expect("encode the author");
    assert_eq!(
        encoded,
        json!({
            "kind": "peer_sidekick",
            "peer": "laptop",
            "fingerprint": "ab12",
            "act": "0198b27e-26ec-7c4c-a83b-a83a4787453f"
        })
    );
    assert_eq!(
        serde_json::from_value::<Author>(encoded).expect("decode the author"),
        on_peer
    );
    let settled = SessionChange::QuestionnaireSettled {
        activity_id: ActivityId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        outcome: QuestionnaireOutcome::Answered,
        answer: None,
        author: Some(on_peer),
        settled_at: Some(SessionTimestamp(9)),
    };
    let encoded = serde_json::to_value(&settled).expect("encode the settling");
    assert_eq!(encoded["settled_at"], json!(9));
    assert_eq!(
        serde_json::from_value::<SessionChange>(encoded).expect("decode the settling"),
        settled
    );
    let asked = Activity::Approval {
        id: ActivityId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        turn_id: TurnId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
        approval: suru::protocol::Approval {
            id: suru::protocol::ApprovalId::from_uuid(fixture_id(
                "0198b27e-4b02-7c4c-a83b-a83a4787453f",
            )),
            subject: ApprovalSubject::Command {
                command: "cargo nextest run".into(),
                cwd: None,
                actions: Vec::new(),
            },
            reason: None,
        },
        tool_activity_id: None,
        detail_truncated: false,
        outcome: suru::protocol::ApprovalOutcome::Pending,
        decision: None,
        follow_up_error: None,
        asked_at: Some(SessionTimestamp(7)),
    };
    let encoded = serde_json::to_value(&asked).expect("encode the Approval");
    assert_eq!(encoded["asked_at"], json!(7));
    assert_eq!(
        serde_json::from_value::<Activity>(encoded).expect("decode the Approval"),
        asked
    );
    // Which Turn had set a Subagent working at any moment is told by when
    // each row leading into it did.
    let delegated = Activity::Subagent {
        id: ActivityId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        turn_id: TurnId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
        status: ActivityStatus::Active,
        name: "Explore".to_owned(),
        description: "Survey the tests".to_owned(),
        model: None,
        session_id: SessionId::from_uuid(fixture_id("0198b27e-4b02-7c4c-a83b-a83a4787453f")),
        brokered: true,
        duration_ms: None,
        delegated_at: Some(SessionTimestamp(5)),
    };
    let encoded = serde_json::to_value(&delegated).expect("encode the row");
    assert_eq!(encoded["delegated_at"], json!(5));
    assert_eq!(
        serde_json::from_value::<Activity>(encoded).expect("decode the row"),
        delegated
    );
}

#[test]
fn a_servers_workspace_listing_carries_the_paths_its_workspaces_are_spelled_in() {
    const {
        assert!(
            PROTOCOL_VERSION >= 77,
            "a Server's listing of its Workspaces is asked of it by a Peer"
        );
    }
    use suru::protocol::{PathStyle, WorkspaceListing, WorkspacePaths};
    let workspace = Workspace::directory(std::env::temp_dir().join("atlas"));
    let listing = WorkspaceListing {
        workspace_paths: WorkspacePaths {
            home: None,
            style: PathStyle::Windows,
            sidekick_workspace: None,
        },
        workspaces: vec![workspace.clone()],
    };
    let encoded = serde_json::to_value(&listing).expect("encode a Workspace listing");
    assert_eq!(
        encoded,
        json!({
            "workspace_paths": { "home": null, "style": "windows" },
            "workspaces": [workspace],
        })
    );
    assert_eq!(
        serde_json::from_value::<WorkspaceListing>(encoded).expect("decode a Workspace listing"),
        listing
    );
}

#[test]
fn a_session_its_server_cannot_read_is_refused_by_a_code_of_its_own() {
    const {
        assert!(
            PROTOCOL_VERSION >= 77,
            "a Session read with its summary, a Peer's request included, is refused so"
        );
    }
    assert_eq!(
        SessionErrorCode::SessionUnreadable.wire_name(),
        "session_unreadable"
    );
}

#[test]
fn a_subsession_names_the_sidekick_that_began_it_and_its_row_names_the_subsession() {
    const {
        assert!(
            PROTOCOL_VERSION >= 74,
            "a Session's beginning author and the Subsession row change the wire, a Remote's \
             included"
        );
        assert!(
            PROTOCOL_VERSION >= 81,
            "the key of the Remote a Subsession row leads onto changes the wire"
        );
    }
    let sidekick = SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f"));
    let subsession = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let workspace = std::env::temp_dir().join("auth");
    let begun = Session {
        begun_by: Some(Author::Sidekick {
            session_id: sidekick,
            title: "Plan the work".to_owned(),
        }),
        ..session_at(subsession, &workspace)
    };
    let encoded = serde_json::to_value(&begun).expect("encode a Subsession");
    assert_eq!(
        encoded["begun_by"],
        json!({
            "kind": "sidekick",
            "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "title": "Plan the work"
        }),
        "a Subsession names the Sidekick that began it as the author a Prompt names"
    );
    assert_eq!(encoded["parent"], json!(null), "and is no one's child");
    assert_eq!(
        serde_json::from_value::<Session>(encoded).expect("decode a Subsession"),
        begun
    );
    assert_eq!(begun.sidekick(), Some(sidekick));
    let users_own = serde_json::to_value(session_at(subsession, &workspace))
        .expect("encode the user's own Session");
    assert!(
        users_own.get("begun_by").is_none(),
        "a Session the user began carries no one on the wire: {users_own}"
    );

    let row = Activity::Subsession {
        id: ActivityId::from_uuid(fixture_id("0198b27e-4b11-7c4c-a83b-a83a4787453f")),
        turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
        session_id: subsession,
        origin: None,
        origin_fingerprint: None,
        title: "Fix the flaky login test".to_owned(),
        prompt: "Fix the flaky login test in the auth suite.".to_owned(),
    };
    let expected = json!({
        "kind": "subsession",
        "id": "0198b27e-4b11-7c4c-a83b-a83a4787453f",
        "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "title": "Fix the flaky login test",
        "prompt": "Fix the flaky login test in the auth suite."
    });
    assert_eq!(
        serde_json::to_value(&row).expect("encode the row"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<Activity>(expected).expect("decode the row"),
        row
    );
    assert_eq!(
        row.status(),
        None,
        "beginning a Subsession is a moment, not work that settles"
    );
    let remotes = Activity::Subsession {
        id: row.id(),
        turn_id: row.turn_id(),
        session_id: subsession,
        origin: Some("workstation".to_owned()),
        origin_fingerprint: Some("ab12cd34ef56".to_owned()),
        title: "Fix the flaky login test".to_owned(),
        prompt: "Fix the flaky login test in the auth suite.".to_owned(),
    };
    let encoded = serde_json::to_value(&remotes).expect("encode a Remote's row");
    assert_eq!(
        (&encoded["origin"], &encoded["origin_fingerprint"]),
        (&json!("workstation"), &json!("ab12cd34ef56")),
        "a row leading onto a Remote names it by its Server's name and by its key, which a \
         reader on another Server knows it by"
    );
    assert_eq!(
        serde_json::from_value::<Activity>(encoded).expect("decode a Remote's row"),
        remotes
    );
    let retitled = SessionChange::SubsessionTitleChanged {
        activity_id: row.id(),
        title: "Flaky login test".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(&retitled).expect("encode the retitling"),
        json!({
            "type": "subsession_title_changed",
            "activity_id": "0198b27e-4b11-7c4c-a83b-a83a4787453f",
            "title": "Flaky login test"
        })
    );
}

#[test]
fn an_answer_a_sidekick_gave_names_the_sidekick_and_the_users_own_names_no_one() {
    const {
        assert!(
            PROTOCOL_VERSION >= 75,
            "an Answer's author changes the wire, a Remote's included"
        );
    }
    let author = Author::Sidekick {
        session_id: SessionId::from_uuid(fixture_id("0198b27e-3a01-7c4c-a83b-a83a4787453f")),
        title: "Tidy the listing".to_owned(),
    };
    let questionnaire = Questionnaire {
        id: QuestionnaireId::from_uuid(fixture_id("0198b27e-4b02-7c4c-a83b-a83a4787453f")),
        questions: vec![Question {
            id: "machine".to_owned(),
            title: None,
            text: "Where should the tests run?".to_owned(),
            choices: Vec::new(),
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    };
    let answer = Answer {
        questions: vec![QuestionAnswer::Freeform {
            text: "Staging".to_owned(),
        }],
    };
    let answered = Activity::Questionnaire {
        id: ActivityId::from_uuid(fixture_id("0198b27e-5c03-7c4c-a83b-a83a4787453f")),
        turn_id: TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d")),
        questionnaire,
        outcome: QuestionnaireOutcome::Answered,
        answer: Some(answer.clone()),
        author: Some(author.clone()),
        asked_at: None,
        settled_at: None,
    };
    let encoded = serde_json::to_value(&answered).expect("encode a Sidekick's Answer");
    assert_eq!(
        encoded["author"],
        json!({
            "kind": "sidekick",
            "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
            "title": "Tidy the listing"
        }),
        "the Questionnaire names who gave its Answer beside the Answer itself"
    );
    assert_eq!(
        serde_json::from_value::<Activity>(encoded).expect("decode a Sidekick's Answer"),
        answered
    );

    let Activity::Questionnaire {
        id,
        turn_id,
        questionnaire,
        outcome,
        answer: given,
        ..
    } = answered
    else {
        unreachable!("the Activity is a Questionnaire");
    };
    let users_own = Activity::Questionnaire {
        id,
        turn_id,
        questionnaire,
        outcome,
        answer: given,
        author: None,
        asked_at: None,
        settled_at: None,
    };
    let encoded = serde_json::to_value(&users_own).expect("encode the user's own Answer");
    assert!(
        encoded.get("author").is_none(),
        "the user's own Answer carries no author on the wire: {encoded}"
    );

    let settled = SessionChange::QuestionnaireSettled {
        activity_id: id,
        outcome: QuestionnaireOutcome::Answered,
        answer: Some(answer),
        author: Some(author),
        settled_at: None,
    };
    let encoded = serde_json::to_value(&settled).expect("encode a Sidekick's settlement");
    assert_eq!(encoded["author"]["kind"], json!("sidekick"));
    assert_eq!(
        serde_json::from_value::<SessionChange>(encoded).expect("decode a Sidekick's settlement"),
        settled
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
                    attachments: Vec::new(),
                    truncated: false,
                    author: None,
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
fn tool_call_activity_lifecycle_uses_typed_incremental_updates() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::ToolCall {
                    id: activity_id,
                    turn_id,
                    status: ActivityStatus::Active,
                    name: "create_issue".to_owned(),
                    server: Some("github".to_owned()),
                    input: String::new(),
                    input_truncated: false,
                    output: String::new(),
                    output_truncated: false,
                    omitted_parts: 0,
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::ToolCallInputChanged {
                activity_id,
                input: "title=Fix the seam".to_owned(),
                input_truncated: true,
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![
                SessionChange::ToolCallOutputAppended {
                    activity_id,
                    content: "Created issue #7".to_owned(),
                },
                SessionChange::ToolCallOutputTruncated { activity_id },
            ],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(11),
            changes: vec![SessionChange::ToolCallStatusChanged {
                activity_id,
                status: ActivityStatus::Failed,
                omitted_parts: 2,
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
                    "kind": "tool_call",
                    "status": "active",
                    "name": "create_issue",
                    "server": "github",
                    "input": "",
                    "input_truncated": false,
                    "output": "",
                    "output_truncated": false,
                    "omitted_parts": 0
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "tool_call_input_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "input": "title=Fix the seam",
                "input_truncated": true
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [
                {
                    "type": "tool_call_output_appended",
                    "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                    "content": "Created issue #7"
                },
                {
                    "type": "tool_call_output_truncated",
                    "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87"
                }
            ]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 11,
            "changes": [{
                "type": "tool_call_status_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "failed",
                "omitted_parts": 2
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode Tool Call Activity updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 4]>(expected)
            .expect("decode Tool Call Activity updates"),
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
fn a_compaction_is_added_active_and_settles_with_its_context_fill_and_summary() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let completed = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let failed = ActivityId::from_uuid(fixture_id("0198b27e-3c11-7f0a-9d55-2b8e01c7a4f2"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::Compaction {
                    id: completed,
                    turn_id,
                    status: ActivityStatus::Active,
                    trigger: CompactionTrigger::Automatic,
                    instructions: None,
                    before_tokens: None,
                    after_tokens: None,
                    error: None,
                    summary: None,
                    summary_truncated: false,
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            // A completed Compaction carries the summary it left, and whether
            // Suru's cap cut it short, as a typed property beside it.
            changes: vec![SessionChange::CompactionSettled {
                activity_id: completed,
                status: ActivityStatus::Completed,
                before_tokens: Some(182_000),
                after_tokens: Some(31_000),
                error: None,
                summary: Some("The parser work is half done.".to_owned()),
                summary_truncated: true,
            }],
        },
        // A side the Provider reported nothing for stays absent, a failure
        // carries the Provider's account of why, and no summary. A manual
        // Compaction carries the user's instructions for its summary from
        // the moment it opens, whatever becomes of it.
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![
                SessionChange::ActivityAdded {
                    activity: Activity::Compaction {
                        id: failed,
                        turn_id,
                        status: ActivityStatus::Active,
                        trigger: CompactionTrigger::Manual,
                        instructions: Some("Keep the parser notes\nand the lexer plan".to_owned()),
                        before_tokens: None,
                        after_tokens: None,
                        error: None,
                        summary: None,
                        summary_truncated: false,
                    },
                },
                SessionChange::CompactionSettled {
                    activity_id: failed,
                    status: ActivityStatus::Failed,
                    before_tokens: Some(182_000),
                    after_tokens: None,
                    error: Some("Conversation too long".to_owned()),
                    summary: None,
                    summary_truncated: false,
                },
            ],
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
                    "kind": "compaction",
                    "status": "active",
                    "trigger": "automatic",
                    "instructions": null,
                    "before_tokens": null,
                    "after_tokens": null,
                    "error": null,
                    "summary": null,
                    "summary_truncated": false
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "compaction_settled",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "completed",
                "before_tokens": 182000,
                "after_tokens": 31000,
                "error": null,
                "summary": "The parser work is half done.",
                "summary_truncated": true
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [
                {
                    "type": "activity_added",
                    "activity": {
                        "id": "0198b27e-3c11-7f0a-9d55-2b8e01c7a4f2",
                        "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                        "kind": "compaction",
                        "status": "active",
                        "trigger": "manual",
                        "instructions": "Keep the parser notes\nand the lexer plan",
                        "before_tokens": null,
                        "after_tokens": null,
                        "error": null,
                        "summary": null,
                        "summary_truncated": false
                    }
                },
                {
                    "type": "compaction_settled",
                    "activity_id": "0198b27e-3c11-7f0a-9d55-2b8e01c7a4f2",
                    "status": "failed",
                    "before_tokens": 182000,
                    "after_tokens": null,
                    "error": "Conversation too long",
                    "summary": null,
                    "summary_truncated": false
                }
            ]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode Compaction updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 3]>(expected).expect("decode Compaction updates"),
        updates
    );
}

#[test]
fn a_completed_compaction_is_measured_after_by_the_sessions_next_context_fill_reading() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(11),
        changes: vec![
            SessionChange::ContextFillChanged {
                context_fill: Some(suru::protocol::ContextFill {
                    occupied_tokens: 35_000,
                    capacity_tokens: Some(272_000),
                }),
            },
            SessionChange::CompactionAfterMeasured {
                activity_id,
                after_tokens: 35_000,
            },
        ],
    };
    let expected = json!({
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "revision": 11,
        "changes": [
            {
                "type": "context_fill_changed",
                "context_fill": {"occupied_tokens": 35000, "capacity_tokens": 272000}
            },
            {
                "type": "compaction_after_measured",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "after_tokens": 35000
            }
        ]
    });

    assert_eq!(
        serde_json::to_value(&update).expect("encode the measurement"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionUpdate>(expected).expect("decode the measurement"),
        update
    );
}

#[test]
fn a_watch_outcome_is_added_already_settled_with_its_status_description_and_summary() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(8),
        changes: vec![SessionChange::ActivityAdded {
            activity: Activity::WatchOutcome {
                id: activity_id,
                turn_id,
                status: suru::protocol::WatchOutcomeStatus::Failed,
                description: "cargo test".to_owned(),
                summary: Some(
                    r#"Background command "cargo test" failed with exit code 1"#.to_owned(),
                ),
            },
        }],
    };
    let expected = json!({
        "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
        "revision": 8,
        "changes": [{
            "type": "activity_added",
            "activity": {
                "id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                "kind": "watch_outcome",
                "status": "failed",
                "description": "cargo test",
                "summary": "Background command \"cargo test\" failed with exit code 1"
            }
        }]
    });

    assert_eq!(
        serde_json::to_value(&update).expect("encode a Watch Outcome"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<SessionUpdate>(expected).expect("decode a Watch Outcome"),
        update
    );
}

#[test]
fn subagent_activity_lifecycle_uses_typed_incremental_updates() {
    let session_id = SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let activity_id = ActivityId::from_uuid(fixture_id("0198b27e-345a-700e-ae3b-d971c57fbe87"));
    let child_session_id = SessionId::from_uuid(fixture_id("0198b27e-4f11-7d80-a4de-3f2a6f6b3a01"));
    let updates = [
        SessionUpdate {
            session_id,
            revision: SessionRevision(8),
            changes: vec![SessionChange::ActivityAdded {
                activity: Activity::Subagent {
                    id: activity_id,
                    turn_id,
                    status: ActivityStatus::Active,
                    name: "Explore".to_owned(),
                    description: "Map the provider seams".to_owned(),
                    model: None,
                    session_id: child_session_id,
                    brokered: false,
                    duration_ms: None,
                    delegated_at: None,
                },
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(9),
            changes: vec![SessionChange::SubagentDescriptionChanged {
                activity_id,
                description: "Reading the orchestration actor".to_owned(),
            }],
        },
        SessionUpdate {
            session_id,
            revision: SessionRevision(10),
            changes: vec![SessionChange::SubagentStatusChanged {
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
                    "kind": "subagent",
                    "status": "active",
                    "name": "Explore",
                    "description": "Map the provider seams",
                    "model": null,
                    "session_id": "0198b27e-4f11-7d80-a4de-3f2a6f6b3a01",
                    "brokered": false,
                    "duration_ms": null
                }
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 9,
            "changes": [{
                "type": "subagent_description_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "description": "Reading the orchestration actor"
            }]
        },
        {
            "session_id": "0198b27e-26ec-7c4c-a83b-a83a4787453f",
            "revision": 10,
            "changes": [{
                "type": "subagent_status_changed",
                "activity_id": "0198b27e-345a-700e-ae3b-d971c57fbe87",
                "status": "completed",
                "duration_ms": 72000
            }]
        }
    ]);

    assert_eq!(
        serde_json::to_value(&updates).expect("encode Subagent Activity updates"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<[SessionUpdate; 3]>(expected)
            .expect("decode Subagent Activity updates"),
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
        session_id: None,
        preparation_id: None,
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: PathBuf::from("/work/suru"),
        },
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    };
    let expected = json!({
        "preparation_id": null,
        "agent_selection": null,
        "execution_directory": { "path": "/work/suru" },
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
fn viewed_command_round_trips_with_its_operation_identity() {
    let command = ViewSessionRequest {
        operation_id: ViewSessionOperationId::from_uuid(fixture_id(
            "0198b27e-4aa1-72dd-9ec8-65398d17ec16",
        )),
    };
    let expected = json!({
        "operation_id": "0198b27e-4aa1-72dd-9ec8-65398d17ec16"
    });

    assert_eq!(
        serde_json::to_value(command).expect("encode Viewed command"),
        expected
    );
    assert_eq!(
        serde_json::from_value::<ViewSessionRequest>(expected).expect("decode Viewed command"),
        command
    );
    assert!(
        serde_json::from_value::<ViewSessionRequest>(json!({
            "operation_id": "0198b27e-4aa1-72dd-9ec8-65398d17ec16",
            "unexpected": true
        }))
        .is_err(),
        "Viewed commands reject unknown fields"
    );
}

#[test]
fn attachment_bindings_round_trip_beside_the_prompt_text_and_stay_off_the_wire_when_absent() {
    let hash = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    let prompt = InitialPrompt {
        id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
        text: "Compare [Image 1]".to_owned(),
        skill_invocations: Vec::new(),
        attachments: vec![AttachmentBinding {
            attachment_id: AttachmentId::new(hash),
            label: "[Image 1]".to_owned(),
            span: TextSpan { start: 8, end: 17 },
        }],
    };
    let encoded = serde_json::to_value(&prompt).expect("encode Attachment-bearing Prompt");

    assert_eq!(
        encoded,
        json!({
            "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "text": "Compare [Image 1]",
            "skill_invocations": [],
            "attachments": [{
                "attachment_id": hash,
                "label": "[Image 1]",
                "span": { "start": 8, "end": 17 }
            }]
        })
    );
    assert_eq!(
        serde_json::from_value::<InitialPrompt>(encoded).expect("decode Attachment-bearing Prompt"),
        prompt
    );

    let plain = json!({
        "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
        "text": "Nothing attached",
        "skill_invocations": []
    });
    let decoded = serde_json::from_value::<InitialPrompt>(plain.clone())
        .expect("a Prompt without bindings decodes");
    assert!(decoded.attachments.is_empty());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), plain);
}

#[test]
fn a_preparation_prompt_carries_its_attachment_bindings_and_keeps_them_off_the_wire_when_absent() {
    const {
        assert!(
            PROTOCOL_VERSION >= 68,
            "a preparation Prompt carrying Attachment bindings changes the wire"
        );
    }
    let hash = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    let request = PrepareCheckoutRequest {
        intended_session: None,
        id: PreparationId(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360fa")),
        source: suru::protocol::ExecutionDirectory {
            path: PathBuf::from("workspace"),
        },
        prompt: PreparationPrompt {
            text: "[Image 1] fix flicker".to_owned(),
            skill_invocations: Vec::new(),
            attachments: vec![AttachmentBinding {
                attachment_id: AttachmentId::new(hash),
                label: "[Image 1]".to_owned(),
                span: TextSpan { start: 0, end: 9 },
            }],
        },
        provider: ProviderId::new("codex"),
    };
    let encoded = serde_json::to_value(&request).expect("encode the preparation request");

    assert_eq!(
        encoded["prompt"],
        json!({
            "text": "[Image 1] fix flicker",
            "skill_invocations": [],
            "attachments": [{
                "attachment_id": hash,
                "label": "[Image 1]",
                "span": { "start": 0, "end": 9 }
            }]
        })
    );
    assert_eq!(
        serde_json::from_value::<PrepareCheckoutRequest>(encoded)
            .expect("decode the preparation request"),
        request
    );

    let plain = json!({ "text": "Fix flicker", "skill_invocations": [] });
    let decoded = serde_json::from_value::<PreparationPrompt>(plain.clone())
        .expect("a preparation Prompt without bindings decodes");
    assert!(decoded.attachments.is_empty());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), plain);
}

#[test]
fn an_attachment_descriptor_carries_a_typed_kind_and_never_the_bytes() {
    let descriptor = AttachmentDescriptor {
        id: AttachmentId::new("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
        kind: AttachmentKind::Image {
            width: 640,
            height: 480,
        },
        mime_type: "image/png".to_owned(),
        byte_length: 4096,
    };
    let encoded = json!({
        "id": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        "kind": { "type": "image", "width": 640, "height": 480 },
        "mime_type": "image/png",
        "byte_length": 4096
    });

    assert_eq!(serde_json::to_value(&descriptor).unwrap(), encoded);
    assert_eq!(
        serde_json::from_value::<AttachmentDescriptor>(encoded.clone()).unwrap(),
        descriptor
    );
    let mut with_bytes = encoded;
    with_bytes["bytes"] = json!("iVBORw0KGgo=");
    assert!(
        serde_json::from_value::<AttachmentDescriptor>(with_bytes).is_err(),
        "a descriptor carrying bytes is refused"
    );
}

#[test]
fn a_snapshot_and_its_updates_describe_the_attachments_its_session_binds() {
    const {
        assert!(
            PROTOCOL_VERSION >= 67,
            "describing Attachments in snapshots and updates changes the wire"
        )
    };
    let screenshot = AttachmentDescriptor {
        id: AttachmentId::new("1d0a0cbb1f6f3c12f06c8d9bd8d5cc3b16ce8f8eafa4fdb8ea3a2cc02b64a1d4"),
        kind: AttachmentKind::Image {
            width: 1280,
            height: 720,
        },
        mime_type: "image/png".to_owned(),
        byte_length: 319_488,
    };
    let encoded_screenshot = json!({
        "id": "1d0a0cbb1f6f3c12f06c8d9bd8d5cc3b16ce8f8eafa4fdb8ea3a2cc02b64a1d4",
        "kind": { "type": "image", "width": 1280, "height": 720 },
        "mime_type": "image/png",
        "byte_length": 319_488
    });

    let described = SessionChange::AttachmentsDescribed {
        attachments: vec![screenshot.clone()],
    };
    let encoded_change = json!({
        "type": "attachments_described",
        "attachments": [encoded_screenshot.clone()]
    });
    assert_eq!(serde_json::to_value(&described).unwrap(), encoded_change);
    assert_eq!(
        serde_json::from_value::<SessionChange>(encoded_change).unwrap(),
        described
    );

    let snapshot = serde_json::from_value::<SessionSnapshot>(json!({
        "title": "",
        "session": serde_json::to_value(Session {
            checkout: None,
            context_fill: None,
            id: SessionId::from_uuid(fixture_id("0198b27e-26ec-7c4c-a83b-a83a4787453f")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: PathBuf::from("/work/suru"),
            },
            workspace: Workspace::directory(PathBuf::from("/work/suru")),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        })
        .unwrap(),
        "revision": 1,
        "prompts": [],
        "turns": [],
        "messages": [],
        "activities": [],
        "transcript": [],
        "attachments": [encoded_screenshot.clone()]
    }))
    .expect("a snapshot carries the descriptors its Session binds");
    assert_eq!(snapshot.attachments, vec![screenshot.clone()]);
    assert_eq!(
        serde_json::to_value(&snapshot).unwrap()["attachments"],
        json!([encoded_screenshot])
    );
    assert_eq!(
        snapshot.attachment(&screenshot.id),
        Some(&screenshot),
        "a client finds a bound Attachment's descriptor by its id"
    );
    assert_eq!(snapshot.attachment(&AttachmentId::new("never-bound")), None);

    let mut bare = serde_json::to_value(&snapshot).unwrap();
    bare.as_object_mut()
        .expect("the encoded snapshot is an object")
        .remove("attachments");
    assert!(
        serde_json::from_value::<SessionSnapshot>(bare)
            .expect("a snapshot binding nothing may leave the list out")
            .attachments
            .is_empty()
    );
}

#[test]
fn prompt_admission_command_round_trips_with_its_client_generated_identity() {
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9")),
            text: "Steer the current Session".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
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
            SessionChange::TurnUsageChanged {
                turn_id,
                usage: Usage {
                    fresh_input_tokens: Some(1_200),
                    output_tokens: Some(900),
                    ..Usage::default()
                },
                cost: Cost::from_usd(0.03),
                cost_basis: Some(CostBasis::Reported),
                cost_coverage: Some(suru::protocol::CostCoverage::Turn),
                cost_is_partial: false,
                cost_recorded_at: Some(SessionTimestamp(1_755_000_004_100)),
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
                    "type": "turn_usage_changed",
                    "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
                    "usage": {
                        "fresh_input_tokens": 1_200,
                        "cache_read_tokens": null,
                        "cache_write_tokens": null,
                        "output_tokens": 900,
                        "reasoning_tokens": null,
                        "native_meter": null
                    },
                    "cost": 0.03,
                    "cost_basis": "reported",
                    "cost_coverage": { "scope": "turn" },
                    "cost_is_partial": false,
                    "cost_recorded_at": 1_755_000_004_100_u64
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
        serde_json::to_value(SessionErrorCode::InterruptionFailed)
            .expect("encode interruption failure code"),
        json!("interruption_failed")
    );
    assert_eq!(
        serde_json::to_value([
            PromptStatus::Pending,
            PromptStatus::Delivered,
            PromptStatus::Failed,
            PromptStatus::Cancelled,
        ])
        .expect("encode Prompt statuses"),
        json!(["pending", "delivered", "failed", "cancelled"])
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

#[test]
fn a_session_error_names_its_code_in_a_header_as_its_body_does() {
    const {
        assert!(
            PROTOCOL_VERSION >= 69,
            "the Attachment HEAD route, and a Session error's code in a header, change the wire"
        );
    }
    assert_eq!(
        suru::protocol::SESSION_ERROR_CODE_HEADER,
        "x-suru-error-code"
    );
    for code in [
        SessionErrorCode::AttachmentNotFound,
        SessionErrorCode::RemoteNotFound,
        SessionErrorCode::InvalidCommand,
    ] {
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            serde_json::Value::String(code.wire_name()),
            "the header names {code:?} as its body does"
        );
    }
    assert_eq!(
        SessionErrorCode::AttachmentNotFound.wire_name(),
        "attachment_not_found"
    );
}

#[test]
fn a_cost_change_carries_the_own_cost_beside_the_tree_cost() {
    const {
        assert!(
            PROTOCOL_VERSION >= 70,
            "carrying a Session's own Cost beside its tree Cost changes the wire"
        );
    }
    let known = |usd| {
        Cost::from_usd(usd).map(|cost| CostTotal {
            cost,
            is_partial: false,
        })
    };
    let change = SessionChange::TotalCostChanged {
        total_cost: known(0.25),
        own_cost: known(0.20),
    };
    let expected = json!({
        "type": "total_cost_changed",
        "total_cost": { "cost": 0.25, "is_partial": false },
        "own_cost": { "cost": 0.20, "is_partial": false }
    });
    assert_eq!(serde_json::to_value(&change).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<SessionChange>(expected).unwrap(),
        change
    );
}

#[test]
fn a_prompt_the_session_withdrew_carries_why_and_the_turn_it_was_held_behind() {
    const {
        assert!(
            PROTOCOL_VERSION >= 73,
            "saying why a Session withdrew a Prompt it held changes the wire"
        );
    }
    let prompt_id = PromptId::from_uuid(fixture_id("0198b27e-2a7e-7562-b80d-54aa50c360f9"));
    let turn_id = TurnId::from_uuid(fixture_id("0198b27e-2dc4-76ba-9895-f43db821fe3d"));
    let withdrawal = PromptWithdrawal::CompactionUnfinished { turn_id };
    let withdrawn = Prompt {
        id: prompt_id,
        text: "Now the lexer".to_owned(),
        skill_invocations: Vec::new(),
        attachments: Vec::new(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(3),
        status: PromptStatus::Cancelled,
        withdrawal: Some(withdrawal),
        author: None,
        taken: None,
    };
    let expected = json!({
        "id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
        "text": "Now the lexer",
        "skill_invocations": [],
        "delivery": "steer",
        "admission_order": 3,
        "status": "cancelled",
        "withdrawal": {
            "reason": "compaction_unfinished",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d"
        }
    });
    assert_eq!(serde_json::to_value(&withdrawn).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<Prompt>(expected).unwrap(),
        withdrawn
    );

    let cancelled = Prompt {
        withdrawal: None,
        ..withdrawn
    };
    let encoded = serde_json::to_value(&cancelled).unwrap();
    assert_eq!(
        encoded.get("withdrawal"),
        None,
        "a Prompt withdrawn for no reason of the Session's own says nothing of one"
    );
    assert_eq!(
        serde_json::from_value::<Prompt>(encoded).unwrap(),
        cancelled
    );

    let change = SessionChange::PromptWithdrawn {
        prompt_id,
        withdrawal,
    };
    let expected = json!({
        "type": "prompt_withdrawn",
        "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
        "withdrawal": {
            "reason": "compaction_unfinished",
            "turn_id": "0198b27e-2dc4-76ba-9895-f43db821fe3d"
        }
    });
    assert_eq!(serde_json::to_value(&change).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<SessionChange>(expected).unwrap(),
        change
    );
    assert!(
        serde_json::from_value::<SessionChange>(json!({
            "type": "prompt_withdrawn",
            "prompt_id": "0198b27e-2a7e-7562-b80d-54aa50c360f9",
            "withdrawal": { "reason": "compaction_unfinished" }
        }))
        .is_err(),
        "a withdrawal behind a Compaction names the Turn it was held behind"
    );
}

#[test]
fn a_compaction_request_and_the_turn_it_begins_travel_on_the_wire() {
    const {
        assert!(
            PROTOCOL_VERSION >= 71,
            "a Compaction request, the Turn it begins, and its refusals change the wire"
        );
        assert!(
            PROTOCOL_VERSION >= 72,
            "refusing to promote a Prompt while a requested Compaction runs changes the wire"
        );
    }
    assert_eq!(
        serde_json::from_value::<CompactSessionRequest>(json!({})).expect("a bare request decodes"),
        CompactSessionRequest { instructions: None }
    );
    assert_eq!(
        serde_json::to_value(CompactSessionRequest {
            instructions: Some("Keep the parser notes".to_owned()),
        })
        .expect("encode a Compaction request"),
        json!({"instructions": "Keep the parser notes"})
    );
    assert!(
        serde_json::from_value::<CompactSessionRequest>(json!({"instruction": "typo"})).is_err(),
        "a request naming anything else is refused rather than read as bare"
    );

    let turn = json!({
        "id": "0198b27e-2dc4-76ba-9895-f43db821fe3d",
        "prompt_id": null,
        "compaction_requested": true,
        "agent": null,
        "status": "active",
        "started_at": null,
        "settled_at": null,
        "usage": null,
        "cost": null,
        "cost_basis": null
    });
    let requested = serde_json::from_value::<Turn>(turn.clone()).expect("decode a requested Turn");
    assert!(requested.compaction_requested && !requested.is_continuation());
    assert!(!requested.accepts_steer(), "it accepts no steer");
    assert_eq!(
        serde_json::to_value(&requested).expect("encode a requested Turn")["compaction_requested"],
        json!(true)
    );
    let mut unmarked = turn.clone();
    unmarked
        .as_object_mut()
        .expect("a Turn is an object")
        .remove("compaction_requested");
    assert!(
        serde_json::from_value::<Turn>(unmarked)
            .expect("a Turn that says nothing of a request decodes")
            .is_continuation(),
        "a Turn no Prompt and no request began is a Continuation"
    );
    let mut prompted = turn;
    prompted["prompt_id"] = json!("0198b27e-2a7e-7562-b80d-54aa50c360f9");
    assert!(
        serde_json::from_value::<Turn>(prompted).is_err(),
        "no Turn is begun by both a Prompt and a Compaction request"
    );

    for (code, name) in [
        (SessionErrorCode::WorkingSession, "working_session"),
        (
            SessionErrorCode::PendingIntervention,
            "pending_intervention",
        ),
        (SessionErrorCode::SubagentSession, "subagent_session"),
        (
            SessionErrorCode::CompactionUnsupported,
            "compaction_unsupported",
        ),
        (
            SessionErrorCode::CompactionInstructionsUnsupported,
            "compaction_instructions_unsupported",
        ),
        (
            SessionErrorCode::CompactionInProgress,
            "compaction_in_progress",
        ),
    ] {
        assert_eq!(code.wire_name(), name, "{code:?} travels as {name}");
    }
}
