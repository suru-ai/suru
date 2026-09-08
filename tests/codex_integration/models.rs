//! Model catalog paging and normalization, and lowering a selection onto a thread.

use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::Value;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AgentSelection, CreateSessionRequest, InitialPrompt, ModelAvailability, ModelId,
        ModelOptionChoiceId, ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionSelection,
        ModelOptionValue, PromptId, PromptStatus, ProviderCatalogStatus, ProviderId,
        ProviderUnavailability, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

const MODEL_CATALOG_CODEX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CODEX_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CODEX_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CODEX_FIXTURE_ATTEMPTS"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*'"cursor":null'*)
      if [ "$attempt" -gt 1 ]; then
        printf '%s\n' '{"id":2,"error":{"code":-32001,"message":"temporary catalog outage"}}'
      else
        printf '%s\n' '{"id":2,"result":{"data":[{"id":"gpt-opaque","displayName":"GPT Fixture","description":"Primary fixture model","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Faster"},{"reasoningEffort":"xhigh","description":"Deepest"}],"defaultReasoningEffort":"xhigh","serviceTiers":[{"id":"flex-native","name":"Flex","description":"Flexible processing"},{"id":"fast-native","name":"Fast","description":"Priority processing"}],"defaultServiceTier":"flex-native","isDefault":true},{"id":"hidden-model","displayName":"Hidden","description":"Not selectable","hidden":true,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":false}],"nextCursor":"opaque-page-2"}}'
      fi
      ;;
    *'"method":"model/list"'*'"cursor":"opaque-page-2"'*)
      printf '%s\n' '{"id":3,"result":{"data":[{"id":"fast-model","displayName":"Fast Fixture","description":"Has independent speed","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[{"id":"fast","name":"Fast","description":"Priority processing"}],"defaultServiceTier":null,"isDefault":false}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-opaque","reasoningEffort":"xhigh","serviceTier":"flex-native"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const MALFORMED_MODEL_CATALOG_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"id":"incomplete"}],"nextCursor":null}}'
      ;;
  esac
done
"#;

const OUTDATED_CODEX_MODEL_CATALOG: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{"userAgent":"suru/0.149.0 (Linux 6; x86_64) codex_cli_rs/0.149.0"}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Fixture model","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":true}],"nextCursor":null}}'
      ;;
  esac
done
"#;

const SELECTED_MODEL_REJECTION: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"selection rejected by fixture","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"model\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const SELECTED_OPTION_REJECTION: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"selected service tier is unavailable","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"serviceTier\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const NON_MODEL_BAD_REQUEST: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"fixture rejected the input","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"input\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const SELECTED_MODEL_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"provider-default"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"native-thread","threadSettings":{"model":"effective-model","effort":"low","serviceTier":"flex"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const THREAD_DEFAULT_OPTIONS_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"provider-default","reasoningEffort":"medium","serviceTier":null}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"native-thread","threadSettings":{"model":"provider-default","serviceTier":"fast"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

#[tokio::test]
async fn codex_model_catalog_is_paginated_normalized_and_kept_across_refresh_failure() {
    let codex = ScriptedCodex::new(MODEL_CATALOG_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-model-catalog").expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-model-catalog")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let initial = client.list_models().await.expect("discover Codex Models");
    assert_eq!(initial.providers.len(), 1);
    let catalog = &initial.providers[0];
    assert_eq!(catalog.provider, ProviderId::new("codex"));
    assert_eq!(catalog.status, ProviderCatalogStatus::Fresh);
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(catalog.models[0].id, ModelId::new("gpt-opaque"));
    assert_eq!(catalog.models[0].display_name, "GPT Fixture");
    assert_eq!(catalog.models[0].availability, ModelAvailability::Available);
    assert_eq!(catalog.models[0].options.len(), 2);
    assert_eq!(
        catalog.models[0].options[0].role,
        ModelOptionRole::ReasoningEffort
    );
    let ModelOptionKind::Select { choices, default } = &catalog.models[0].options[0].kind else {
        panic!("reasoning effort is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["low", "xhigh"]
    );
    assert_eq!(default.as_str(), "xhigh");
    assert_eq!(catalog.models[0].options[1].role, ModelOptionRole::Speed);
    let ModelOptionKind::Select { choices, default } = &catalog.models[0].options[1].kind else {
        panic!("speed is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["flex-native", "fast-native"]
    );
    assert_eq!(default.as_str(), "flex-native");
    assert_eq!(
        catalog.models[0].default_agent_selection().options,
        vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("xhigh"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("flex-native"),
                },
            },
        ]
    );

    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let incomplete = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-opaque"),
                options: vec![ModelOptionSelection {
                    id: ModelOptionId::new("reasoning_effort"),
                    value: ModelOptionValue::Select {
                        choice: ModelOptionChoiceId::new("low"),
                    },
                }],
            }),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reject incomplete advertised options".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect_err("reject an incomplete advertised Agent Selection");
    assert!(incomplete.to_string().contains("service_tier"));

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Materialize every advertised default".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session from cached advertised defaults");
    assert_eq!(
        created.session.agent_selection,
        Some(catalog.models[0].default_agent_selection())
    );
    assert_eq!(catalog.models[1].options[0].role, ModelOptionRole::Speed);
    let ModelOptionKind::Select { choices, default } = &catalog.models[1].options[0].kind else {
        panic!("speed is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["default", "fast"]
    );
    assert_eq!(default.as_str(), "default");

    let cached = client.list_models().await.expect("read cached catalog");
    assert_eq!(
        cached.providers[0].status,
        ProviderCatalogStatus::Refreshing
    );
    assert_eq!(cached.providers[0].models, catalog.models);
    let stale = client
        .refresh_models()
        .await
        .expect("observe failed background refresh");
    assert_eq!(stale.providers[0].models, catalog.models);
    assert!(matches!(
        &stale.providers[0].status,
        ProviderCatalogStatus::Stale { message } if message.contains("temporary catalog outage")
    ));

    let requests = codex.requests();
    assert!(requests.iter().any(|request| {
        request.get("method").and_then(Value::as_str) == Some("model/list")
            && request["params"]["cursor"] == Value::String("opaque-page-2".to_owned())
    }));
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_outdated_codex_cli_warns_without_withholding_its_models() {
    let codex = ScriptedCodex::new(OUTDATED_CODEX_MODEL_CATALOG);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "outdated-codex-model-catalog")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "outdated-codex-model-catalog")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let catalog = client.list_models().await.expect("request Model catalog");
    let codex = &catalog.providers[0];
    assert_eq!(codex.models.len(), 1, "the warning is non-blocking");
    assert!(matches!(
        &codex.status,
        ProviderCatalogStatus::Warning { message }
            if message.contains("0.149.0")
                && message.contains("0.150.1 or newer")
                && message.contains("may have compatibility issues")
    ));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn malformed_codex_model_results_are_reported_for_the_codex_provider() {
    let codex = ScriptedCodex::new(MALFORMED_MODEL_CATALOG_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-malformed-model-catalog")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-malformed-model-catalog")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let catalog = client.list_models().await.expect("request Model catalog");
    assert_eq!(catalog.providers[0].provider, ProviderId::new("codex"));
    assert!(catalog.providers[0].models.is_empty());
    assert!(matches!(
        &catalog.providers[0].status,
        ProviderCatalogStatus::Failed { message }
            if message.contains("invalid model/list response")
    ));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn selected_model_is_lowered_to_codex_and_effective_model_is_projected_back() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-model").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-model")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let requested = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("requested-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("fast"),
                },
            },
        ],
    };
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(requested),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the requested native Model".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create selected Codex Session");
    fixture.wait_for_method("turn/start").await;
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture native turn/start");
    assert_eq!(turn_start["params"]["model"], "requested-model");
    assert_eq!(turn_start["params"]["effort"], "high");
    assert_eq!(turn_start["params"]["serviceTier"], "fast");

    let completed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read selected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("effective Codex Model is projected");
    let effective = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("effective-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("flex"),
                },
            },
        ],
    };
    assert_eq!(completed.session.agent_selection, Some(effective.clone()));
    assert_eq!(
        completed.turns[0]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&effective)
    );
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("requested-model") && text.contains("effective-model")
    )));
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("reasoning_effort")
                && text.contains("high")
                && text.contains("low")
    )));
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("service_tier")
                && text.contains("fast")
                && text.contains("flex")
    )));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_materializes_thread_options_and_handles_native_omission_and_clear() {
    let fixture = ScriptedCodex::new(THREAD_DEFAULT_OPTIONS_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-thread-default-options")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-thread-default-options")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use every effective default".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create default Codex Session");
    let completed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read default Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("default Codex Turn completes");
    let expected = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("provider-default"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("medium"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("fast"),
                },
            },
        ],
    };
    assert_eq!(completed.session.agent_selection, Some(expected.clone()));
    assert_eq!(
        completed.turns[0]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&expected)
    );
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture default turn/start");
    assert_eq!(turn_start["params"]["effort"], "medium");
    assert!(
        turn_start["params"]
            .as_object()
            .is_some_and(|params| params.get("serviceTier") == Some(&Value::Null))
    );
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("service_tier")
                && text.contains("default")
                && text.contains("fast")
    )));
    assert!(!completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. } if text.contains("reasoning_effort")
    )));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_adapter_distinguishes_explicit_option_defaults_from_native_omission() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-option-defaults").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-option-defaults")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("requested-model"),
                options: vec![
                    ModelOptionSelection {
                        id: ModelOptionId::new("reasoning_effort"),
                        value: ModelOptionValue::Select {
                            choice: ModelOptionChoiceId::new("medium"),
                        },
                    },
                    ModelOptionSelection {
                        id: ModelOptionId::new("service_tier"),
                        value: ModelOptionValue::Select {
                            choice: ModelOptionChoiceId::new("default"),
                        },
                    },
                ],
            }),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Apply explicit defaults".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session with explicit defaults");
    fixture.wait_for_method_count("turn/start", 1).await;
    let explicit = fixture
        .requests()
        .into_iter()
        .find(|request| {
            request["method"] == "turn/start"
                && request["params"]["input"][0]["text"] == "Apply explicit defaults"
        })
        .expect("capture explicit-default turn/start");
    assert_eq!(explicit["params"]["effort"], "medium");
    assert!(
        explicit["params"]
            .as_object()
            .is_some_and(|params| { params.get("serviceTier") == Some(&Value::Null) })
    );

    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("requested-model"),
                options: Vec::new(),
            }),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Leave options omitted".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session without advertised options");
    fixture.wait_for_method_count("turn/start", 2).await;
    let omitted = fixture
        .requests()
        .into_iter()
        .find(|request| {
            request["method"] == "turn/start"
                && request["params"]["input"][0]["text"] == "Leave options omitted"
        })
        .expect("capture omitted-options turn/start");
    let omitted = omitted["params"]
        .as_object()
        .expect("turn/start params are an object");
    assert!(!omitted.contains_key("effort"));
    assert!(!omitted.contains_key("serviceTier"));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_lowers_every_advertised_effort_and_tier_combination_independently() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-option-combinations").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-option-combinations")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    for effort in ["low", "xhigh"] {
        for service_tier in ["flex-native", "fast-native"] {
            client
                .create_session(CreateSessionRequest {
                    preparation_id: None,
                    agent_selection: Some(AgentSelection {
                        provider: ProviderId::new("codex"),
                        model: ModelId::new("gpt-opaque"),
                        options: vec![
                            ModelOptionSelection {
                                id: ModelOptionId::new("reasoning_effort"),
                                value: ModelOptionValue::Select {
                                    choice: ModelOptionChoiceId::new(effort),
                                },
                            },
                            ModelOptionSelection {
                                id: ModelOptionId::new("service_tier"),
                                value: ModelOptionValue::Select {
                                    choice: ModelOptionChoiceId::new(service_tier),
                                },
                            },
                        ],
                    }),
                    execution_directory: suru::protocol::ExecutionDirectory {
                        path: workspace.path().to_owned(),
                    },
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: format!("Use {effort} with {service_tier}"),
                        skill_invocations: Vec::new(),
                    },
                })
                .await
                .expect("create Session for an advertised option combination");
        }
    }
    fixture.wait_for_method_count("turn/start", 4).await;
    let mut combinations = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .map(|request| {
            (
                request["params"]["effort"]
                    .as_str()
                    .expect("effort is a string")
                    .to_owned(),
                request["params"]["serviceTier"]
                    .as_str()
                    .expect("service tier is a string")
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    combinations.sort();
    assert_eq!(
        combinations,
        [
            ("low".to_owned(), "fast-native".to_owned()),
            ("low".to_owned(), "flex-native".to_owned()),
            ("xhigh".to_owned(), "fast-native".to_owned()),
            ("xhigh".to_owned(), "flex-native".to_owned()),
        ]
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_model_rejection_never_falls_back_and_restores_the_prompt() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_REJECTION);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-model-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-model-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("rejected-model"),
        options: Vec::new(),
    };
    let prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(selection.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Do not silently fall back".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read rejected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Codex rejection becomes visible");
    let turn_starts = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 1);
    assert_eq!(turn_starts[0]["params"]["model"], "rejected-model");
    assert_eq!(failed.session.agent_selection, Some(selection));
    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Unavailable
    );
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text.contains("selection rejected by fixture")
    )));
    assert_eq!(failed.prompts.len(), 2);
    assert_eq!(failed.prompts[0].id, prompt_id);
    assert_ne!(failed.prompts[1].id, prompt_id);
    assert_eq!(failed.prompts[1].text, "Do not silently fall back");
    assert_eq!(failed.prompts[1].status, PromptStatus::Pending);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_option_rejection_never_falls_back_and_restores_the_prompt() {
    let fixture = ScriptedCodex::new(SELECTED_OPTION_REJECTION);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-option-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-option-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("valid-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("retired-tier"),
                },
            },
        ],
    };
    let prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(selection.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Do not silently replace the selected speed".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read rejected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Codex option rejection becomes visible");
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture rejected turn/start");
    assert_eq!(turn_start["params"]["model"], "valid-model");
    assert_eq!(turn_start["params"]["effort"], "high");
    assert_eq!(turn_start["params"]["serviceTier"], "retired-tier");
    assert_eq!(failed.session.agent_selection, Some(selection.clone()));
    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Unavailable
    );
    assert_eq!(
        failed.turns[0].agent.as_ref().map(|agent| &agent.selection),
        Some(&selection)
    );
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text.contains("service tier is unavailable")
    )));
    assert_eq!(failed.prompts.len(), 2);
    assert_eq!(failed.prompts[0].id, prompt_id);
    assert_ne!(failed.prompts[1].id, prompt_id);
    assert_eq!(
        failed.prompts[1].text,
        "Do not silently replace the selected speed"
    );
    assert_eq!(failed.prompts[1].status, PromptStatus::Pending);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn generic_codex_turn_rejection_does_not_mark_the_model_unavailable() {
    let fixture = ScriptedCodex::new(NON_MODEL_BAD_REQUEST);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-generic-turn-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-generic-turn-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("valid-model"),
                options: Vec::new(),
            }),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Fail for a reason unrelated to Model selection".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read failed Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generic rejection becomes visible");

    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Available
    );
    assert_eq!(failed.prompts.len(), 1);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Issue #116: a Codex CLI that isn't installed is a condition the user fixes
/// outside Suru, so the catalog reports it as typed unavailability and the next
/// refresh clears it without a restart.
#[tokio::test]
async fn a_missing_codex_binary_is_reported_as_a_provider_that_is_not_installed() {
    let codex = ScriptedCodex::new(MODEL_CATALOG_CODEX);
    let uninstalled = codex.executable().with_extension("uninstalled");
    std::fs::rename(codex.executable(), &uninstalled).expect("uninstall the scripted Codex");
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-not-installed").expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-not-installed")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let missing = client
        .list_models()
        .await
        .expect("the catalog reports the condition rather than failing the call");
    let ProviderCatalogStatus::Unavailable { reason, message } = &missing.providers[0].status
    else {
        panic!(
            "a missing Codex binary is typed unavailability, got {:?}",
            missing.providers[0].status
        );
    };
    assert_eq!(*reason, ProviderUnavailability::NotInstalled);
    assert!(
        message.contains("could not launch"),
        "the reason keeps Codex's own account of the condition, got {message:?}"
    );
    assert!(missing.providers[0].models.is_empty());

    std::fs::rename(&uninstalled, codex.executable()).expect("install the scripted Codex");
    let installed = client
        .refresh_models()
        .await
        .expect("refresh once the CLI is installed");
    assert_eq!(installed.providers[0].status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        installed.providers[0].models[0].id,
        ModelId::new("gpt-opaque"),
        "the installed Provider serves its Models without a restart"
    );

    server.shutdown().await.expect("shut down server");
}
