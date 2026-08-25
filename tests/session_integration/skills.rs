//! Skill Catalog discovery and authoritative Prompt admission.

use std::sync::Arc;

use crate::{
    provider_support::ControlledProvider,
    server_support::next_skill_catalog,
    support::{hosted_model, hosted_selection, read_session_at_least_revision},
};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        AgentId, AgentIdentity, CreateSessionRequest, InitialPrompt, PromptId, ProviderId,
        SessionError, SessionErrorCode, SessionRevision, SettingMutation, SkillCatalog,
        SkillCatalogCapabilities, SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillInvocation, SkillMarkerSpan, SkillPromptDelivery, Workspace,
    },
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

async fn list_skills_until(
    client: &reqwest::Client,
    descriptor: &suru::protocol::RuntimeDescriptor,
    request: &SkillCatalogRequest,
    ready: impl Fn(&SkillCatalogStatus) -> bool,
) -> SkillCatalog {
    timeout(Duration::from_secs(1), async {
        loop {
            let catalog = client
                .post(format!("{}/v1/skills", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .json(request)
                .send()
                .await
                .expect("list Skills while awaiting state")
                .error_for_status()
                .expect("Skill Catalog state remains readable")
                .json::<SkillCatalog>()
                .await
                .expect("decode Skill Catalog state");
            if ready(&catalog.status) {
                return catalog;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Skill Catalog reaches expected state")
}

#[tokio::test]
async fn cached_catalog_invalidation_is_failure_isolated_and_pushed_to_every_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create Workspace");
    let canonical_workspace =
        std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace");
    let (runtime, _provider) = ControlledProvider::with_provider(
        ProviderId::new("controlled"),
        vec![hosted_model("controlled", "controlled-model")],
    );
    let original = SkillDescriptor {
        id: SkillId::new("original-review-id"),
        name: "review".to_owned(),
        description: "Review the current change".to_owned(),
        scope: Some("Workspace".to_owned()),
    };
    runtime.offer_skills(SkillCatalog {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: canonical_workspace.clone(),
        },
        skills: vec![original.clone()],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    });
    let channel = "live-skill-catalog-test";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new((*runtime).clone()),
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure second client"),
    )
    .await
    .expect("connect second client");
    crate::support::receive_managed_client_initial_state(&mut first).await;
    crate::support::receive_managed_client_initial_state(&mut second).await;
    let request = SkillCatalogRequest {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };

    let loading = first
        .list_skills(request.clone())
        .await
        .expect("prefetch Skill Catalog");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let first_fresh = next_skill_catalog(&mut first).await;
    let second_fresh = next_skill_catalog(&mut second).await;
    assert_eq!(first_fresh, second_fresh);
    assert_eq!(first_fresh.skills, vec![original.clone()]);
    assert_eq!(runtime.skill_discoveries(), 1);
    assert!(matches!(
        first
            .list_skills(request.clone())
            .await
            .expect("read cached Skill Catalog")
            .status,
        SkillCatalogStatus::Fresh { .. }
    ));
    assert_eq!(
        runtime.skill_discoveries(),
        1,
        "fresh reads use server authority"
    );

    runtime.fail_skill_discovery("private Provider failure");
    runtime.invalidate_skill_catalog();
    assert!(matches!(
        next_skill_catalog(&mut first).await.status,
        SkillCatalogStatus::Refreshing
    ));
    assert!(matches!(
        next_skill_catalog(&mut second).await.status,
        SkillCatalogStatus::Refreshing
    ));
    let first_stale = next_skill_catalog(&mut first).await;
    let second_stale = next_skill_catalog(&mut second).await;
    assert_eq!(first_stale, second_stale);
    assert_eq!(first_stale.skills, vec![original.clone()]);
    assert!(matches!(
        first_stale.status,
        SkillCatalogStatus::Stale { .. }
    ));

    let replacement = SkillDescriptor {
        id: SkillId::new("replacement-review-id"),
        ..original.clone()
    };
    runtime.clear_skill_discovery_failure();
    runtime.offer_skills(SkillCatalog {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: canonical_workspace,
        },
        skills: vec![replacement.clone()],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    });
    let refreshing = first
        .refresh_skills(request.clone())
        .await
        .expect("retry Skill discovery");
    assert!(matches!(refreshing.status, SkillCatalogStatus::Refreshing));
    let _ = next_skill_catalog(&mut first).await;
    let _ = next_skill_catalog(&mut second).await;
    let replaced = next_skill_catalog(&mut first).await;
    let also_replaced = next_skill_catalog(&mut second).await;
    assert_eq!(replaced, also_replaced);
    assert_eq!(replaced.skills, vec![replacement]);
    assert!(matches!(replaced.status, SkillCatalogStatus::Fresh { .. }));

    let rejected = reqwest::Client::new()
        .post(format!("{}/v1/sessions", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .json(&CreateSessionRequest {
            agent_selection: Some(hosted_selection("controlled", "controlled-model")),
            workspace: request.workspace,
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review".to_owned(),
                skill_invocations: vec![SkillInvocation {
                    skill_id: original.id,
                    name: original.name,
                    scope: original.scope,
                    marker: SkillMarkerSpan { start: 0, end: 7 },
                }],
            },
        })
        .send()
        .await
        .expect("submit stale Skill binding");
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    runtime.offer_skills(SkillCatalog {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace"),
        },
        skills: vec![SkillDescriptor {
            id: SkillId::new("valid-partial-id"),
            name: "explain".to_owned(),
            description: "Explain the change".to_owned(),
            scope: Some("Workspace".to_owned()),
        }],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh {
            warning: Some("2 invalid Skills were skipped".to_owned()),
        },
    });
    first
        .refresh_skills(SkillCatalogRequest {
            provider: ProviderId::new("controlled"),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("refresh to partial result");
    let _ = next_skill_catalog(&mut first).await;
    let _ = next_skill_catalog(&mut second).await;
    let partial = next_skill_catalog(&mut first).await;
    let also_partial = next_skill_catalog(&mut second).await;
    assert_eq!(partial, also_partial);
    assert_eq!(partial.skills[0].name, "explain");
    assert!(matches!(
        partial.status,
        SkillCatalogStatus::Fresh { warning: Some(_) }
    ));

    drop(first);
    drop(second);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn force_refresh_during_discovery_discards_the_superseded_result() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create Workspace");
    let canonical_workspace =
        std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace");
    let (runtime, _provider) = ControlledProvider::with_provider(
        ProviderId::new("controlled"),
        vec![hosted_model("controlled", "controlled-model")],
    );
    let catalog = |id: &str| SkillCatalog {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: canonical_workspace.clone(),
        },
        skills: vec![SkillDescriptor {
            id: SkillId::new(id),
            name: "review".to_owned(),
            description: id.to_owned(),
            scope: Some("Workspace".to_owned()),
        }],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    };
    runtime.offer_skills(catalog("superseded"));
    let release = runtime.block_next_skill_discovery();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "skill-discovery-race-test").expect("configure server"),
        Arc::new((*runtime).clone()),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "skill-discovery-race-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    let request = SkillCatalogRequest {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };

    assert!(matches!(
        client
            .list_skills(request.clone())
            .await
            .expect("start discovery")
            .status,
        SkillCatalogStatus::Loading
    ));
    timeout(Duration::from_secs(1), async {
        while runtime.skill_discoveries() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial discovery starts");
    runtime.offer_skills(catalog("replacement"));
    assert!(matches!(
        client
            .refresh_skills(request)
            .await
            .expect("queue force refresh")
            .status,
        SkillCatalogStatus::Loading
    ));
    release.send(()).expect("release initial discovery");

    let fresh = next_skill_catalog(&mut client).await;
    assert_eq!(fresh.skills[0].id, SkillId::new("replacement"));
    assert_eq!(runtime.skill_discoveries(), 2);
    assert_eq!(runtime.skill_refreshes(), 1);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn catalog_cache_is_scoped_by_provider_and_canonical_workspace() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace_parent = tempfile::tempdir().expect("create Workspace parent");
    let first_workspace = workspace_parent.path().join("first");
    let second_workspace = workspace_parent.path().join("second");
    std::fs::create_dir(&first_workspace).expect("create first Workspace");
    std::fs::create_dir(&second_workspace).expect("create second Workspace");
    let first_canonical =
        std::fs::canonicalize(&first_workspace).expect("canonicalize first Workspace");
    let second_canonical =
        std::fs::canonicalize(&second_workspace).expect("canonicalize second Workspace");
    let (alpha, _alpha_provider) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-model")],
    );
    let (beta, _beta_provider) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-model")],
    );
    let catalog = |provider: &str, workspace: std::path::PathBuf, id: &str| SkillCatalog {
        provider: ProviderId::new(provider),
        workspace: Workspace { path: workspace },
        skills: vec![SkillDescriptor {
            id: SkillId::new(id),
            name: "review".to_owned(),
            description: format!("{provider} {id}"),
            scope: Some("Workspace".to_owned()),
        }],
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![SkillPromptDelivery::Initial],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    };
    alpha.offer_skills(catalog("alpha", first_canonical.clone(), "alpha-first"));
    beta.offer_skills(catalog("beta", first_canonical.clone(), "beta-first"));
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "skill-cache-scope-test").expect("configure server"),
        vec![Arc::new((*alpha).clone()), Arc::new((*beta).clone())],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let alpha_first = SkillCatalogRequest {
        provider: ProviderId::new("alpha"),
        workspace: Workspace {
            path: workspace_parent.path().join(".").join("first"),
        },
    };
    let beta_first = SkillCatalogRequest {
        provider: ProviderId::new("beta"),
        workspace: Workspace {
            path: first_workspace.clone(),
        },
    };
    let _ = list_skills_until(&client, &descriptor, &alpha_first, |status| {
        matches!(status, SkillCatalogStatus::Fresh { .. })
    })
    .await;
    let beta_catalog = list_skills_until(&client, &descriptor, &beta_first, |status| {
        matches!(status, SkillCatalogStatus::Fresh { .. })
    })
    .await;
    assert_eq!(beta_catalog.skills[0].id, SkillId::new("beta-first"));

    alpha.offer_skills(catalog("alpha", second_canonical, "alpha-second"));
    let alpha_second = SkillCatalogRequest {
        provider: ProviderId::new("alpha"),
        workspace: Workspace {
            path: second_workspace,
        },
    };
    let second_catalog = list_skills_until(&client, &descriptor, &alpha_second, |status| {
        matches!(status, SkillCatalogStatus::Fresh { .. })
    })
    .await;
    assert_eq!(second_catalog.skills[0].id, SkillId::new("alpha-second"));

    let first_again = list_skills_until(&client, &descriptor, &alpha_first, |status| {
        matches!(status, SkillCatalogStatus::Fresh { .. })
    })
    .await;
    assert_eq!(first_again.skills[0].id, SkillId::new("alpha-first"));
    assert_eq!(alpha.skill_discoveries(), 2);
    assert_eq!(beta.skill_discoveries(), 1);

    server.shutdown().await.expect("shut down server");
}

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
    let loading: SkillCatalog = catalog_response
        .json()
        .await
        .expect("decode loading Skill Catalog");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let request = SkillCatalogRequest {
        provider: ProviderId::new("controlled"),
        workspace: Workspace {
            path: workspace.clone(),
        },
    };
    let catalog = list_skills_until(&client, &server_descriptor, &request, |status| {
        matches!(status, SkillCatalogStatus::Fresh { .. })
    })
    .await;
    let catalog_text = serde_json::to_string(&catalog).expect("encode safe Skill Catalog");
    assert!(!catalog_text.contains("SKILL.md"));
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
    let (runtime, mut provider) = ControlledProvider::with_provider(
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
    let loading = client
        .post(format!("{}/v1/skills", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("list failing Codex Skills");
    assert_eq!(loading.status(), reqwest::StatusCode::OK);
    let failed = list_skills_until(&client, &descriptor, &request, |status| {
        matches!(status, SkillCatalogStatus::Unavailable { .. })
    })
    .await;
    let body = serde_json::to_string(&failed).expect("encode failed Skill Catalog");
    assert!(!body.contains("/private/codex"));
    assert!(body.contains("discovery failed"));
    assert_eq!(runtime.skill_discoveries(), 1);

    let ordinary = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: Some(hosted_selection("codex", "controlled-model")),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue without an explicit Skill".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("submit ordinary Prompt while Skills are unavailable");
    assert_eq!(ordinary.status(), reqwest::StatusCode::CREATED);
    provider
        .next_start()
        .await
        .fail("ordinary Provider use remains independent");

    server.shutdown().await.expect("shut down server");
}
