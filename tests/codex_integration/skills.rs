//! Codex-native Skill discovery and structured Prompt lowering.

use crate::server_support::PROGRESS_DEADLINE;
use std::sync::Arc;

use crate::{
    server_support::{
        attachments::{bound, jpeg, png, uploaded},
        next_skill_catalog,
    },
    support::{ScriptedCodex, receive_initial_state},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        Activity, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery,
        PromptId, PromptStatus, SessionSnapshot, SessionStatus, SkillCatalogStatus,
        SkillDescriptor, SkillInvocation, TextSpan, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const SKILL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"skills/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"cwd":$SKILL_WORKSPACE,"skills":[{"name":"review","description":"Review the current change","path":"/private/codex/skills/review/SKILL.md","scope":"repo","enabled":true},{"name":"explain","description":"Explain the current change","path":"/private/codex/skills/explain/SKILL.md","scope":"user","enabled":true},{"name":"disabled","description":"Not offered","path":"/private/codex/skills/disabled/SKILL.md","scope":"user","enabled":false}],"errors":[]}]}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"skill-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"skill-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"skill-thread","turn":{"id":"skill-turn","status":"completed","items":[]}}}'
      ;;
"#;

const CHANGING_SKILL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"skills/list"'*)
      case "$line" in
        *'"forceReload":true'*)
          printf '%s\n' '{"id":2,"result":{"data":[{"cwd":$SKILL_WORKSPACE,"skills":[{"name":"review","description":"Replacement","path":"/private/codex/skills/replacement/SKILL.md","scope":"repo","enabled":true}],"errors":[]}]}}'
          ;;
        *)
          printf '%s\n' '{"id":2,"result":{"data":[{"cwd":$SKILL_WORKSPACE,"skills":[{"name":"review","description":"Original","path":"/private/codex/skills/original/SKILL.md","scope":"repo","enabled":true}],"errors":[]}]}}'
          ;;
      esac
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"skill-change-thread"},"model":"gpt-fixture"}}'
      printf '%s\n' '{"method":"skills/changed","params":{}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"skill-change-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"skill-change-thread","turn":{"id":"skill-change-turn","status":"completed","items":[]}}}'
      ;;
"#;

const SKILL_OPERATION_CODEX: &str = r#"#!/bin/sh
turn_index=0
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"skills/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"cwd":$SKILL_WORKSPACE,"skills":[{"name":"review","description":"Review the current change","path":"/private/codex/skills/review/SKILL.md","scope":"repo","enabled":true},{"name":"explain","description":"Explain the current change","path":"/private/codex/skills/explain/SKILL.md","scope":"user","enabled":true}],"errors":[]}]}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"skill-operation-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
__TURN_START_ACTION__
      ;;
    *'"method":"turn/steer"'*)
__STEER_ACTION__
      ;;
  esac
done
wait
"#;

const DELIVER_SKILL_TURNS: &str = r#"      turn_index=$((turn_index + 1))
      if [ "$turn_index" -eq 1 ]; then
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"skill-operation-turn-1"}}}'
        (
          wait_for "$CODEX_FIXTURE_RELEASE"
          printf '%s\n' '{"method":"turn/completed","params":{"threadId":"skill-operation-thread","turn":{"id":"skill-operation-turn-1","status":"completed","items":[]}}}'
        ) &
      else
        printf '%s\n' '{"id":6,"result":{"turn":{"id":"skill-operation-turn-2"}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"skill-operation-thread","turn":{"id":"skill-operation-turn-2","status":"completed","items":[]}}}'
      fi"#;

const ACCEPT_SKILL_STEER: &str =
    r#"      printf '%s\n' '{"id":5,"result":{"turnId":"skill-operation-turn-1"}}'"#;

const REJECT_SKILL_TURN: &str = r#"      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"fixture rejected structured Skill input"}}'"#;

const UNEXPECTED_SKILL_STEER: &str = "      exit 65";

fn skill_operation_script(
    workspace: &std::path::Path,
    turn_start_action: &str,
    steer_action: &str,
) -> String {
    let workspace_json = serde_json::to_string(workspace).expect("encode Workspace");
    SKILL_OPERATION_CODEX
        .replace("$SKILL_WORKSPACE", &workspace_json)
        .replace("__TURN_START_ACTION__", turn_start_action)
        .replace("__STEER_ACTION__", steer_action)
}

fn invocation(skill: &SkillDescriptor, start: u32, end: u32) -> SkillInvocation {
    SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        span: TextSpan { start, end },
    }
}

async fn wait_for_snapshot(
    client: &ManagedClient,
    session_id: suru::protocol::SessionId,
    description: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Codex Skill Session");
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"))
}

#[tokio::test]
async fn codex_skill_changes_force_refresh_server_authority() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let workspace_json = serde_json::to_string(&canonical_workspace).expect("encode Workspace");
    let fixture = ScriptedCodex::new_multiprocess(
        &CHANGING_SKILL_CODEX.replace("$SKILL_WORKSPACE", &workspace_json),
    );
    let channel = "codex-skill-change-test";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::SettingsSnapshot(_))
    ));
    let request = suru::protocol::SkillCatalogRequest {
        provider: suru::protocol::ProviderId::new("codex"),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.path().to_owned(),
        },
    };
    assert!(matches!(
        client
            .list_skills(request)
            .await
            .expect("prefetch Codex Skills")
            .status,
        SkillCatalogStatus::Loading
    ));
    let original = next_skill_catalog(&mut client).await;
    assert_eq!(original.skills[0].description, "Original");
    let original_id = original.skills[0].id.clone();

    client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Notice native Skill changes".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Codex Session");

    assert!(matches!(
        next_skill_catalog(&mut client).await.status,
        SkillCatalogStatus::Refreshing
    ));
    let replacement = next_skill_catalog(&mut client).await;
    assert!(matches!(
        replacement.status,
        SkillCatalogStatus::Fresh { .. }
    ));
    assert_eq!(replacement.skills[0].description, "Replacement");
    assert_ne!(replacement.skills[0].id, original_id);
    let refresh = fixture.requests().into_iter().find(|request| {
        request.get("method").and_then(Value::as_str) == Some("skills/list")
            && request["params"]["forceReload"] == true
    });
    assert!(
        refresh.is_some(),
        "Codex invalidation forces native refresh"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_delivers_ordered_distinct_skills_with_visible_skill_only_transcript() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let workspace_json = serde_json::to_string(&canonical_workspace).expect("encode Workspace");
    let fixture =
        ScriptedCodex::new_multiprocess(&SKILL_CODEX.replace("$SKILL_WORKSPACE", &workspace_json));
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-skill-test").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-skill-test").expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let loading = client
        .list_skills(suru::protocol::SkillCatalogRequest {
            provider: suru::protocol::ProviderId::new("codex"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: canonical_workspace,
            },
        })
        .await
        .expect("list Codex Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    assert!(matches!(
        catalog.status,
        SkillCatalogStatus::Fresh { warning: None }
    ));
    assert_eq!(catalog.skills.len(), 2, "disabled Skills stay native-only");
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered")
        .clone();
    let explain = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "explain")
        .expect("explain Skill is offered")
        .clone();
    assert_eq!(review.scope.as_deref(), Some("Workspace"));
    assert_eq!(explain.scope.as_deref(), Some("User"));
    assert_eq!(catalog.capabilities.max_distinct_invocations, None);
    assert_eq!(
        catalog.capabilities.supported_deliveries,
        [
            suru::protocol::SkillPromptDelivery::Initial,
            suru::protocol::SkillPromptDelivery::Queue,
            suru::protocol::SkillPromptDelivery::Steer,
        ]
    );
    let serialized = serde_json::to_string(&catalog).expect("serialize safe Skill Catalog");
    assert!(!serialized.contains("SKILL.md"));
    assert!(!serialized.contains("/private/codex"));

    let visible_prompt = "$review\n$explain $review";
    let invocations = vec![
        SkillInvocation {
            skill_id: review.id.clone(),
            name: review.name.clone(),
            scope: review.scope.clone(),
            span: TextSpan { start: 0, end: 7 },
        },
        SkillInvocation {
            skill_id: explain.id.clone(),
            name: explain.name.clone(),
            scope: explain.scope.clone(),
            span: TextSpan { start: 8, end: 16 },
        },
        SkillInvocation {
            skill_id: review.id,
            name: review.name,
            scope: review.scope,
            span: TextSpan { start: 17, end: 24 },
        },
    ];
    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: visible_prompt.to_owned(),
                skill_invocations: invocations.clone(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Codex Skill Session");

    fixture.wait_for_method("turn/start").await;
    let started = fixture
        .requests()
        .into_iter()
        .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
        .expect("Codex receives the Turn");
    assert_eq!(
        started["params"]["input"],
        json!([
            { "type": "text", "text": visible_prompt },
            {
                "type": "skill",
                "name": "review",
                "path": "/private/codex/skills/review/SKILL.md"
            },
            {
                "type": "skill",
                "name": "explain",
                "path": "/private/codex/skills/explain/SKILL.md"
            }
        ])
    );

    let delivered = wait_for_snapshot(
        &client,
        created.session.id,
        "Codex Skill Prompt reaches the Transcript",
        |snapshot| !snapshot.messages.is_empty(),
    )
    .await;
    assert_eq!(delivered.prompts[0].text, visible_prompt);
    assert_eq!(delivered.prompts[0].skill_invocations, invocations);
    assert_eq!(delivered.messages[0].content, visible_prompt);
    assert_eq!(
        delivered.messages[0].skill_invocations,
        delivered.prompts[0].skill_invocations
    );
    let serialized = serde_json::to_string(&delivered).expect("serialize client Session data");
    assert!(!serialized.contains("SKILL.md"));
    assert!(!serialized.contains("/private/codex"));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_preserves_skill_bindings_through_queue_and_steer_delivery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let fixture = ScriptedCodex::new(&skill_operation_script(
        &canonical_workspace,
        DELIVER_SKILL_TURNS,
        ACCEPT_SKILL_STEER,
    ));
    let channel = "codex-skill-queue-steer-test";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let loading = client
        .list_skills(suru::protocol::SkillCatalogRequest {
            provider: suru::protocol::ProviderId::new("codex"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: canonical_workspace,
            },
        })
        .await
        .expect("list Codex Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered");
    let explain = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "explain")
        .expect("explain Skill is offered");

    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Hold the active Turn".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create active Codex Session");
    fixture.wait_for_method_count("turn/start", 1).await;
    wait_for_snapshot(
        &client,
        created.session.id,
        "initial Codex Turn becomes active",
        // A Session reads as Active from the moment its Prompt is admitted, so
        // what this waits for is the Turn itself (ADR 0024).
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Active)
        },
    )
    .await;

    let queued_text = "$explain\n$review $explain";
    let queued_invocations = vec![
        invocation(explain, 0, 8),
        invocation(review, 9, 16),
        invocation(explain, 17, 25),
    ];
    let queued = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: queued_text.to_owned(),
                    skill_invocations: queued_invocations.clone(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit queued Codex Skill Prompt");
    assert_eq!(queued.status, PromptStatus::Pending);

    let steer_text = "$review\n$explain $review";
    let steer_invocations = vec![
        invocation(review, 0, 7),
        invocation(explain, 8, 16),
        invocation(review, 17, 24),
    ];
    let steer = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: steer_text.to_owned(),
                    skill_invocations: steer_invocations.clone(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit Codex Skill steer");
    fixture.wait_for_method("turn/steer").await;
    let steered = wait_for_snapshot(
        &client,
        created.session.id,
        "Codex Skill steer reaches the Transcript",
        |snapshot| {
            snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == steer.id)
                .is_some_and(|prompt| prompt.status == PromptStatus::Delivered)
        },
    )
    .await;
    let steer_message = steered
        .messages
        .iter()
        .find(|message| message.content == steer_text)
        .expect("delivered steer has a visible Message");
    assert_eq!(steer_message.skill_invocations, steer_invocations);

    let steer_request = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/steer")
        .expect("Codex receives the steer");
    assert_eq!(
        steer_request["params"]["input"],
        json!([
            { "type": "text", "text": steer_text },
            {
                "type": "skill",
                "name": "review",
                "path": "/private/codex/skills/review/SKILL.md"
            },
            {
                "type": "skill",
                "name": "explain",
                "path": "/private/codex/skills/explain/SKILL.md"
            }
        ])
    );

    fixture.release();
    fixture.wait_for_method_count("turn/start", 2).await;
    let completed = wait_for_snapshot(
        &client,
        created.session.id,
        "queued Codex Skill Turn settles",
        |snapshot| snapshot.session.status == SessionStatus::Idle && snapshot.turns.len() == 2,
    )
    .await;
    let queued_message = completed
        .messages
        .iter()
        .find(|message| message.content == queued_text)
        .expect("queued Prompt has a visible Message");
    assert_eq!(queued_message.skill_invocations, queued_invocations);

    let queued_start = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .nth(1)
        .expect("Codex receives the queued Turn");
    assert_eq!(
        queued_start["params"]["input"],
        json!([
            { "type": "text", "text": queued_text },
            {
                "type": "skill",
                "name": "explain",
                "path": "/private/codex/skills/explain/SKILL.md"
            },
            {
                "type": "skill",
                "name": "review",
                "path": "/private/codex/skills/review/SKILL.md"
            }
        ])
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// The `turn/start` or `turn/steer` input item an image Attachment reaches Codex as.
fn data_url_image(mime_type: &str, bytes: &[u8]) -> Value {
    json!({
        "type": "image",
        "url": format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
    })
}

#[tokio::test]
async fn codex_receives_attachments_as_data_url_images_after_the_text_on_start_and_steer() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let fixture = ScriptedCodex::new(&skill_operation_script(
        &canonical_workspace,
        DELIVER_SKILL_TURNS,
        ACCEPT_SKILL_STEER,
    ));
    let channel = "codex-attachment-delivery-test";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let loading = client
        .list_skills(suru::protocol::SkillCatalogRequest {
            provider: suru::protocol::ProviderId::new("codex"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: canonical_workspace,
            },
        })
        .await
        .expect("list Codex Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered");
    let descriptor = server.descriptor().clone();
    let screenshot_bytes = png(640, 480);
    let photo_bytes = jpeg(1920, 1080);
    let screenshot = uploaded(&descriptor, screenshot_bytes.clone()).await;
    let photo = uploaded(&descriptor, photo_bytes.clone()).await;

    let initial_text = "$review compare [Image 1] with [Image 2]";
    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: initial_text.to_owned(),
                skill_invocations: vec![invocation(review, 0, 7)],
                // Bound out of label order: Codex receives them in the order their labels stand.
                attachments: vec![
                    bound(&photo, initial_text, "[Image 2]"),
                    bound(&screenshot, initial_text, "[Image 1]"),
                ],
            },
        })
        .await
        .expect("create a Codex Session with two images");
    fixture.wait_for_method_count("turn/start", 1).await;
    wait_for_snapshot(
        &client,
        created.session.id,
        "the Turn the images began becomes active",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Active)
        },
    )
    .await;

    let steer_text = "And [Image 1] once more";
    let steer = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: steer_text.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: vec![bound(&screenshot, steer_text, "[Image 1]")],
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Codex Turn with an image");
    fixture.wait_for_method("turn/steer").await;
    wait_for_snapshot(
        &client,
        created.session.id,
        "the image steer is delivered",
        |snapshot| {
            snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
        },
    )
    .await;

    let requests = fixture.requests();
    let start = requests
        .iter()
        .find(|request| request["method"] == "turn/start")
        .expect("Codex receives the Turn start");
    assert_eq!(
        start["params"]["input"],
        json!([
            { "type": "text", "text": initial_text },
            {
                "type": "skill",
                "name": "review",
                "path": "/private/codex/skills/review/SKILL.md"
            },
            data_url_image("image/png", &screenshot_bytes),
            data_url_image("image/jpeg", &photo_bytes),
        ]),
        "the images follow the text, in label order, as data URLs"
    );
    let steer_request = requests
        .iter()
        .find(|request| request["method"] == "turn/steer")
        .expect("Codex receives the steer");
    assert_eq!(
        steer_request["params"]["input"],
        json!([
            { "type": "text", "text": steer_text },
            data_url_image("image/png", &screenshot_bytes),
        ])
    );
    for item in requests
        .iter()
        .filter(|request| request["method"] == "turn/start" || request["method"] == "turn/steer")
        .flat_map(|request| request["params"]["input"].as_array().expect("input items"))
    {
        assert_ne!(
            item["type"], "localImage",
            "no local path ever reaches Codex"
        );
        if item["type"] == "image" {
            assert!(
                item["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("data:")),
                "no remote URL ever reaches Codex: {item}"
            );
        }
    }

    fixture.release();
    wait_for_snapshot(
        &client,
        created.session.id,
        "the image Turn settles",
        |snapshot| snapshot.session.status == SessionStatus::Idle,
    )
    .await;
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_does_not_retry_rejected_structured_skills_as_plain_text() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let fixture = ScriptedCodex::new(&skill_operation_script(
        &canonical_workspace,
        REJECT_SKILL_TURN,
        UNEXPECTED_SKILL_STEER,
    ));
    let channel = "codex-skill-rejection-test";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let loading = client
        .list_skills(suru::protocol::SkillCatalogRequest {
            provider: suru::protocol::ProviderId::new("codex"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: canonical_workspace,
            },
        })
        .await
        .expect("list Codex Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered");
    let visible_prompt = "$review";
    let invocations = vec![invocation(review, 0, 7)];

    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: visible_prompt.to_owned(),
                skill_invocations: invocations.clone(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create rejected Codex Skill Session");
    fixture.wait_for_method("turn/start").await;
    let failed = wait_for_snapshot(
        &client,
        created.session.id,
        "structured Skill rejection fails the Turn",
        |snapshot| {
            snapshot.session.status == SessionStatus::Idle
                && snapshot.activities.iter().any(|activity| {
                    matches!(activity, Activity::Error { text, .. }
                        if text.contains("fixture rejected structured Skill input"))
                })
        },
    )
    .await;
    assert_eq!(failed.messages[0].content, visible_prompt);
    assert_eq!(failed.messages[0].skill_invocations, invocations);
    let serialized = serde_json::to_string(&failed).expect("serialize failed Session data");
    assert!(!serialized.contains("SKILL.md"));
    assert!(!serialized.contains("/private/codex"));

    let starts = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1, "Codex receives no text-only retry");
    assert_eq!(
        starts[0]["params"]["input"],
        json!([
            { "type": "text", "text": visible_prompt },
            {
                "type": "skill",
                "name": "review",
                "path": "/private/codex/skills/review/SKILL.md"
            }
        ])
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
