//! Codex-native Skill discovery and structured Prompt lowering.

use std::sync::Arc;

use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::{Value, json};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, SkillCatalog, SkillCatalogStatus,
        SkillInvocation, SkillMarkerSpan, Workspace,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};

const SKILL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"skills/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"cwd":$SKILL_WORKSPACE,"skills":[{"name":"review","description":"Review the current change","path":"/private/codex/skills/review/SKILL.md","scope":"repo","enabled":true},{"name":"disabled","description":"Not offered","path":"/private/codex/skills/disabled/SKILL.md","scope":"user","enabled":false}],"errors":[]}]}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"skill-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"skill-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"skill-thread","turn":{"id":"skill-turn","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn codex_discovers_enabled_skills_and_receives_structured_input_with_visible_text() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace");
    let workspace_json = serde_json::to_string(&canonical_workspace).expect("encode Workspace");
    let fixture =
        ScriptedCodex::new_multiprocess(&SKILL_CODEX.replace("$SKILL_WORKSPACE", &workspace_json));
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-skill-test").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-skill-test").expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let catalog = reqwest::Client::new()
        .post(format!("{}/v1/skills", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&json!({
            "provider": "codex",
            "workspace": { "path": canonical_workspace }
        }))
        .send()
        .await
        .expect("list Codex Skills")
        .error_for_status()
        .expect("Codex Skill listing succeeds")
        .json::<SkillCatalog>()
        .await
        .expect("decode Codex Skill Catalog");
    assert!(matches!(
        catalog.status,
        SkillCatalogStatus::Fresh { warning: None }
    ));
    assert_eq!(catalog.skills.len(), 1, "disabled Skills stay native-only");
    let skill = catalog.skills[0].clone();
    assert_eq!(skill.name, "review");
    assert_eq!(skill.scope.as_deref(), Some("Workspace"));
    assert_eq!(catalog.capabilities.max_distinct_invocations, Some(1));
    assert_eq!(
        catalog.capabilities.supported_deliveries,
        [suru::protocol::SkillPromptDelivery::Initial]
    );
    let serialized = serde_json::to_string(&catalog).expect("serialize safe Skill Catalog");
    assert!(!serialized.contains("SKILL.md"));
    assert!(!serialized.contains("/private/codex"));

    client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review improve this".to_owned(),
                skill_invocations: vec![SkillInvocation {
                    skill_id: skill.id,
                    name: skill.name,
                    scope: skill.scope,
                    marker: SkillMarkerSpan { start: 0, end: 7 },
                }],
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
            { "type": "text", "text": "$review improve this" },
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
