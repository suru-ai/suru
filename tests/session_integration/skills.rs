//! Skill Catalog discovery and authoritative Prompt admission.

use std::sync::Arc;

use crate::{
    provider_support::ControlledProvider,
    support::{hosted_model, hosted_selection, read_session_at_least_revision},
};
use serde_json::json;
use suru::{
    protocol::{
        AgentId, AgentIdentity, CreateSessionRequest, InitialPrompt, PromptId, ProviderId,
        SessionError, SessionErrorCode, SessionRevision, SettingMutation, SkillCatalog,
        SkillCatalogCapabilities, SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillInvocation, SkillMarkerSpan, SkillPromptDelivery, Workspace,
    },
    server::{self, ServerConfig},
};

#[tokio::test]
async fn server_lists_and_admits_only_the_current_workspace_skill() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace_parent = tempfile::tempdir().expect("create Workspace parent");
    let workspace = workspace_parent.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create Workspace");
    let canonical_workspace = std::fs::canonicalize(&workspace).expect("canonicalize Workspace");
    let model = hosted_model("controlled", "controlled-model");
    let selection = hosted_selection("controlled", "controlled-model");
    let (runtime, mut provider) =
        ControlledProvider::with_provider(ProviderId::new("controlled"), vec![model]);
    let descriptor = SkillDescriptor {
        id: SkillId::new("opaque-review-id"),
        name: "review".to_owned(),
        description: "Review the current change".to_owned(),
        scope: Some("Workspace".to_owned()),
    };
    runtime.offer_skills(SkillCatalog {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: canonical_workspace.clone(),
        },
        skills: vec![descriptor.clone()],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    });
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "skill-admission-test").expect("configure server"),
        Arc::new((*runtime).clone()),
    )
    .await
    .expect("spawn server");
    let server_descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let catalog_response = client
        .post(format!("{}/v1/skills", server_descriptor.base_url))
        .bearer_auth(&server_descriptor.token)
        .json(&json!({
            "provider": "controlled",
            "workspace": { "path": workspace_parent.path().join(".").join("workspace") }
        }))
        .send()
        .await
        .expect("list Skills");
    assert_eq!(catalog_response.status(), reqwest::StatusCode::OK);
    let catalog_text = catalog_response.text().await.expect("read Skill Catalog");
    assert!(!catalog_text.contains("SKILL.md"));
    let catalog: SkillCatalog = serde_json::from_str(&catalog_text).expect("decode Skill Catalog");
    assert_eq!(catalog.workspace.path, canonical_workspace);
    assert_eq!(catalog.skills, vec![descriptor.clone()]);

    let invocation = SkillInvocation {
        skill_id: descriptor.id.clone(),
        name: descriptor.name.clone(),
        scope: descriptor.scope.clone(),
        marker: SkillMarkerSpan { start: 0, end: 7 },
    };
    let prompt_id = PromptId::new();
    let created_response = client
        .post(format!("{}/v1/sessions", server_descriptor.base_url))
        .bearer_auth(&server_descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: Some(selection.clone()),
            workspace: Workspace {
                path: workspace.clone(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "$review".to_owned(),
                skill_invocations: vec![invocation.clone()],
            },
        })
        .send()
        .await
        .expect("create Skill-only Session");
    assert_eq!(created_response.status(), reqwest::StatusCode::CREATED);
    let created = created_response
        .json::<suru::protocol::SessionSnapshot>()
        .await
        .expect("decode created Session");

    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection,
    });
    let turn = provider_session.next_turn().await;
    assert_eq!(turn.prompt(), "$review");
    assert_eq!(turn.skill_invocations().len(), 1);
    assert_eq!(turn.skill_invocations()[0].skill_id, descriptor.id);
    turn.succeed();

    let delivered = read_session_at_least_revision(
        &client,
        &server_descriptor,
        created.session.id,
        SessionRevision(2),
    )
    .await;
    assert_eq!(delivered.messages[0].content, "$review");
    assert_eq!(delivered.messages[0].skill_invocations, vec![invocation]);

    let rejected = client
        .post(format!("{}/v1/sessions", server_descriptor.base_url))
        .bearer_auth(&server_descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: Some(hosted_selection("controlled", "controlled-model")),
            workspace: Workspace { path: workspace },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review".to_owned(),
                skill_invocations: vec![SkillInvocation {
                    skill_id: SkillId::new("forged-id"),
                    name: "review".to_owned(),
                    scope: Some("Workspace".to_owned()),
                    marker: SkillMarkerSpan { start: 0, end: 7 },
                }],
            },
        })
        .send()
        .await
        .expect("submit forged Skill binding");
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        rejected
            .json::<SessionError>()
            .await
            .expect("decode Skill admission error")
            .code,
        SessionErrorCode::InvalidSkillInvocation
    );
    assert!(provider.try_next_start().is_none());

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn disabled_providers_are_not_discovered_and_native_discovery_errors_are_redacted() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, _provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "controlled-model")],
    );
    runtime.fail_skill_discovery("could not read /private/codex/skills/review/SKILL.md");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "skill-safety-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        Arc::new((*runtime).clone()),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    client
        .post(format!("{}/v1/settings", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&SettingMutation::ProviderCodexEnabled { value: Some(false) })
        .send()
        .await
        .expect("disable Codex")
        .error_for_status()
        .expect("Codex disablement succeeds");
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };

    let disabled = client
        .post(format!("{}/v1/skills", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("list disabled Codex Skills")
        .error_for_status()
        .expect("disabled Skill Catalog remains readable")
        .json::<SkillCatalog>()
        .await
        .expect("decode disabled Skill Catalog");
    assert!(matches!(
        disabled.status,
        SkillCatalogStatus::Unavailable { .. }
    ));
    assert_eq!(runtime.skill_discoveries(), 0);

    client
        .post(format!("{}/v1/settings", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&SettingMutation::ProviderCodexEnabled { value: Some(true) })
        .send()
        .await
        .expect("enable Codex")
        .error_for_status()
        .expect("Codex enablement succeeds");
    let failed = client
        .post(format!("{}/v1/skills", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("list failing Codex Skills");
    assert_eq!(failed.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body = failed.text().await.expect("read redacted discovery error");
    assert!(!body.contains("/private/codex"));
    assert!(body.contains("could not list Skills"));
    assert_eq!(runtime.skill_discoveries(), 1);

    server.shutdown().await.expect("shut down server");
}
