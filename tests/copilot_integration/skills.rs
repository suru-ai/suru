//! Copilot-native Skill discovery and command expansion.

use std::sync::Arc;

use crate::{
    server_support::{next_skill_catalog, receive_initial_state},
    support::{
        ScriptedCopilot, connect_arm, create_session_arm, current_model_arm, delete_session_arm,
        destroy_session_arm, permission_decision_arm, send_arm, session_where, settled_session,
        signed_in_arm, upgradable_connect_arm,
    },
};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery,
        PromptId, PromptStatus, ProviderId, SessionStatus, SkillCatalogRequest, SkillCatalogStatus,
        SkillId, SkillInvocation, SkillMarkerSpan, SkillPromptDelivery, Workspace,
    },
    provider::CopilotRuntime,
    server::{self, ServerConfig},
};

fn command_catalog_arm() -> &'static str {
    r#"    *'"method":"session.commands.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"commands":[{"name":"native-review","description":"Native description stays private","kind":"skill","allowDuringAgentExecution":true},{"name":"help","description":"Show help","kind":"builtin","allowDuringAgentExecution":true},{"name":"extension-command","description":"Extension command","kind":"client","allowDuringAgentExecution":false}]}}'
      ;;
    *'"method":"session.skills.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"skills":[{"name":"review","description":"Review the current change","source":"project","enabled":true,"userInvocable":true,"commandName":"native-review","path":"/private/copilot/review/SKILL.md"},{"name":"disabled","description":"Not offered","source":"personal-copilot","enabled":false,"userInvocable":true,"commandName":"native-disabled","path":"/private/copilot/disabled/SKILL.md"},{"name":"automatic","description":"Agent-only","source":"plugin","enabled":true,"userInvocable":false,"commandName":"native-automatic","pluginName":"private-plugin"}]}}'
      ;;
"#
}

fn partially_invalid_command_catalog_arm() -> &'static str {
    r#"    *'"method":"session.commands.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"commands":[{"name":"native-review","description":"Private review command","kind":"skill","allowDuringAgentExecution":true},{"name":"native-invalid","description":"Private invalid command","kind":"skill","allowDuringAgentExecution":true}]}}'
      ;;
    *'"method":"session.skills.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"skills":[{"name":"review","description":"Review the current change","source":"project","enabled":true,"userInvocable":true,"commandName":"native-review","path":"private-review"},{"name":"invalid name","description":"Malformed Skill","source":"project","enabled":true,"userInvocable":true,"commandName":"native-invalid","path":"private-invalid"},{"name":"orphaned","description":"Missing command metadata","source":"custom","enabled":true,"userInvocable":true,"commandName":"native-missing","path":"private-missing"}]}}'
      ;;
"#
}

#[tokio::test]
async fn an_incompatible_pinned_cli_reports_copilot_skills_actionably_unavailable() {
    let copilot = ScriptedCopilot::new(&upgradable_connect_arm());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "copilot-skill-incompatible-handshake";
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
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
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("copilot"),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("list Copilot Skills against incompatible CLI");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let SkillCatalogStatus::Unavailable { message } = catalog.status else {
        panic!("incompatible pinned CLI makes Copilot Skills unavailable");
    };
    assert!(message.contains("update the Copilot CLI"));
    assert_eq!(catalog.capabilities.max_distinct_invocations, Some(0));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

fn two_skill_command_catalog_arm() -> &'static str {
    r#"    *'"method":"session.commands.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"commands":[{"name":"native-review","description":"Private review command","kind":"skill","allowDuringAgentExecution":true},{"name":"native-explain","description":"Private explain command","kind":"skill","allowDuringAgentExecution":true}]}}'
      ;;
    *'"method":"session.skills.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"skills":[{"name":"review","description":"Review the current change","source":"project","enabled":true,"userInvocable":true,"commandName":"native-review","path":"/private/copilot/review/SKILL.md"},{"name":"explain","description":"Explain the current change","source":"personal-copilot","enabled":true,"userInvocable":true,"commandName":"native-explain","path":"/private/copilot/explain/SKILL.md"}]}}'
      ;;
"#
}

fn command_invocation_arm() -> &'static str {
    r#"    *'"method":"session.commands.invoke"'*)
      case "$body" in
        *'"input":""'*)
          reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"kind":"agent-prompt","displayPrompt":"Review","prompt":"EXPANDED EMPTY"}}'
          ;;
        *)
          reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"kind":"agent-prompt","displayPrompt":"Review","prompt":"EXPANDED REQUEST"}}'
          ;;
      esac
      ;;
"#
}

fn queued_and_steer_invocation_arm() -> &'static str {
    r#"    *'"method":"session.commands.invoke"'*)
      case "$body" in
        *'Steer  now'*) expanded='EXPANDED STEER' ;;
        *'Queue  later'*) expanded='EXPANDED QUEUE' ;;
        *) exit 67 ;;
      esac
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"kind":"agent-prompt","displayPrompt":"Review","prompt":"'"$expanded"'"}}'
      ;;
"#
}

fn held_then_queued_send_arm() -> &'static str {
    r#"    *'"method":"session.send"'*)
      sid=$(printf '%s' "$body" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"messageId":"fixture-message"}}'
      case "$body" in
        *'"mode":"immediate"'*) ;;
        *)
          sends=$(( ${sends:-0} + 1 ))
          if [ "$sends" -eq 1 ]; then
            (
              while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
              event initial-idle session.idle '{}'
            ) &
          else
            event queued-idle session.idle '{}'
          fi
          ;;
      esac
      ;;
"#
}

fn unavailable_command_catalog_arm() -> &'static str {
    r#"    *'"method":"session.commands.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32601,"message":"unknown experimental method"}}'
      ;;
"#
}

fn rejected_command_invocation_arm() -> &'static str {
    r#"    *'"method":"session.commands.invoke"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fixture rejected native Skill invocation"}}'
      ;;
"#
}

fn changing_nonsteer_skill_catalog_arms() -> &'static str {
    r#"    *'"method":"session.commands.list"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"commands":[{"name":"native-review","description":"Private command","kind":"skill","allowDuringAgentExecution":false}]}}'
      ;;
    *'"method":"session.skills.list"'*)
      skill_lists=$(( ${skill_lists:-0} + 1 ))
      if [ "$skill_lists" -eq 1 ]; then source_path='/private/copilot/original/SKILL.md'; else source_path='/private/copilot/replacement/SKILL.md'; fi
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"skills":[{"name":"review","description":"Review the current change","source":"project","enabled":true,"userInvocable":true,"commandName":"native-review","path":"'"$source_path"'"}]}}'
      ;;
"#
}

async fn host_skills(
    copilot: &ScriptedCopilot,
    channel: &'static str,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    server::RunningServer,
    ManagedClient,
) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    (state_dir, workspace, server, client)
}

async fn fresh_catalog(
    client: &mut ManagedClient,
    workspace: &std::path::Path,
) -> suru::protocol::SkillCatalog {
    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("copilot"),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
        })
        .await
        .expect("list Copilot Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(client).await;
    assert!(matches!(
        catalog.status,
        SkillCatalogStatus::Fresh { warning: None }
    ));
    catalog
}

#[tokio::test]
async fn copilot_lists_only_native_skills_with_stable_opaque_identity_and_one_skill_limit() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        command_catalog_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-catalog";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let request = SkillCatalogRequest {
        provider: ProviderId::new("copilot"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };

    let first = fresh_catalog(&mut client, workspace.path()).await;
    assert_eq!(
        first
            .skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str()))
            .collect::<Vec<_>>(),
        [("review", "Review the current change")],
        "built-in and client-owned commands stay outside the Skill Catalog"
    );
    assert!(first.skills[0].id.as_str().starts_with("copilot-"));
    assert_ne!(first.skills[0].id.as_str(), "review");
    assert_eq!(first.capabilities.max_distinct_invocations, Some(1));
    assert_eq!(
        first.capabilities.supported_deliveries,
        [
            SkillPromptDelivery::Initial,
            SkillPromptDelivery::Queue,
            SkillPromptDelivery::Steer,
        ]
    );
    let serialized = serde_json::to_string(&first).expect("serialize safe Copilot Skill Catalog");
    assert!(!serialized.contains("native-review"));
    assert!(!serialized.contains("/private/copilot"));
    assert!(!serialized.contains("private-plugin"));

    let refreshing = client
        .refresh_skills(request)
        .await
        .expect("refresh Copilot Skills");
    assert!(matches!(refreshing.status, SkillCatalogStatus::Refreshing));
    let mut refreshed = next_skill_catalog(&mut client).await;
    if matches!(refreshed.status, SkillCatalogStatus::Refreshing) {
        refreshed = next_skill_catalog(&mut client).await;
    }
    assert!(matches!(
        refreshed.status,
        SkillCatalogStatus::Fresh { warning: None }
    ));
    assert_eq!(refreshed.skills[0].id, first.skills[0].id);

    let listings = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.commands.list")
        .collect::<Vec<_>>();
    assert_eq!(listings.len(), 2);
    for listing in listings {
        assert_eq!(listing["params"]["includeBuiltins"], false);
        assert_eq!(listing["params"]["includeClientCommands"], false);
        assert_eq!(listing["params"]["includeSkills"], true);
    }
    let creates = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.create")
        .collect::<Vec<_>>();
    assert_eq!(creates.len(), 2);
    assert!(creates.iter().all(|request| {
        request["params"]["workingDirectory"].as_str()
            == std::fs::canonicalize(workspace.path())
                .expect("canonicalize Workspace")
                .to_str()
    }));
    assert!(
        creates
            .iter()
            .all(|request| request["params"]["enableSkills"] == true)
    );
    assert!(
        creates
            .iter()
            .all(|request| request["params"]["enableConfigDiscovery"] == true)
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_partial_native_catalog_keeps_valid_skills_with_an_aggregate_warning() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        partially_invalid_command_catalog_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-partial-catalog";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;

    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("copilot"),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("list partially valid Copilot Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    assert_eq!(
        catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["review"]
    );
    let SkillCatalogStatus::Fresh {
        warning: Some(warning),
    } = catalog.status
    else {
        panic!("partial Copilot Skill discovery should retain an aggregate warning");
    };
    assert!(warning.contains("2 invalid Skill entries"));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn copilot_invokes_one_skill_with_the_full_marker_free_input_and_sends_only_its_agent_prompt()
{
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        command_catalog_arm(),
        command_invocation_arm(),
        send_arm("      event sent session.idle '{}'\n"),
        permission_decision_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-invocation";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    let skill = catalog.skills.first().expect("review Skill is offered");

    let cases = [
        ("Please $review this", SkillMarkerSpan { start: 7, end: 14 }),
        ("$review", SkillMarkerSpan { start: 0, end: 7 }),
    ];
    for (index, (text, marker)) in cases.into_iter().enumerate() {
        let invocation = SkillInvocation {
            skill_id: skill.id.clone(),
            name: skill.name.clone(),
            scope: skill.scope.clone(),
            marker,
        };
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: vec![invocation.clone()],
                },
            })
            .await
            .expect("create Copilot Skill Session");
        let settled = settled_session(&client, created.session.id, 0).await;
        assert_eq!(settled.messages[0].content, text);
        assert_eq!(settled.messages[0].skill_invocations, [invocation]);

        let invokes = copilot
            .requests()
            .into_iter()
            .filter(|request| request["method"] == "session.commands.invoke")
            .collect::<Vec<_>>();
        let invoke = &invokes[index];
        assert_eq!(invoke["params"]["name"], "native-review");
        assert_eq!(
            invoke["params"]["input"],
            if index == 0 {
                json!("Please  this")
            } else {
                json!("")
            }
        );
    }

    let sends = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.send")
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["params"]["prompt"], "EXPANDED REQUEST");
    assert_eq!(sends[1]["params"]["prompt"], "EXPANDED EMPTY");
    assert!(sends.iter().all(|request| {
        !request["params"]["prompt"]
            .as_str()
            .expect("sent Prompt is text")
            .contains("$review")
    }));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn copilot_expands_queued_and_steer_skills_before_using_each_native_delivery_mode() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        command_catalog_arm(),
        queued_and_steer_invocation_arm(),
        held_then_queued_send_arm(),
        permission_decision_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-queue-steer";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    let skill = catalog.skills.first().expect("review Skill is offered");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Hold this Turn".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create active Copilot Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Copilot Skill Session");
    session_where(
        &client,
        &mut feed,
        created.session.id,
        "the first Copilot Turn becomes active",
        |snapshot| snapshot.session.status == SessionStatus::Active,
    )
    .await;

    let queued_text = "Queue $review later";
    let queued_invocation = SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        marker: SkillMarkerSpan { start: 6, end: 13 },
    };
    let queued = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: queued_text.to_owned(),
                    skill_invocations: vec![queued_invocation.clone()],
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue Copilot Skill Prompt");
    assert_eq!(queued.status, PromptStatus::Pending);

    let steer_text = "Steer $review now";
    let steer_invocation = SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        marker: SkillMarkerSpan { start: 6, end: 13 },
    };
    let steer = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: steer_text.to_owned(),
                    skill_invocations: vec![steer_invocation.clone()],
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer with Copilot Skill Prompt");
    session_where(
        &client,
        &mut feed,
        created.session.id,
        "the expanded Copilot Skill steer is delivered",
        |snapshot| {
            snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
        },
    )
    .await;

    copilot.release();
    let completed = session_where(
        &client,
        &mut feed,
        created.session.id,
        "the queued Copilot Skill Prompt runs after the first Turn",
        |snapshot| snapshot.turns.len() == 2 && snapshot.session.status == SessionStatus::Idle,
    )
    .await;
    let queued_message = completed
        .messages
        .iter()
        .find(|message| message.content == queued_text)
        .expect("queued Prompt remains visible");
    assert_eq!(queued_message.skill_invocations, [queued_invocation]);
    let steer_message = completed
        .messages
        .iter()
        .find(|message| message.content == steer_text)
        .expect("steer Prompt remains visible");
    assert_eq!(steer_message.skill_invocations, [steer_invocation]);

    let invokes = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.commands.invoke")
        .collect::<Vec<_>>();
    assert_eq!(invokes.len(), 2);
    assert_eq!(invokes[0]["params"]["input"], "Steer  now");
    assert_eq!(invokes[1]["params"]["input"], "Queue  later");
    let sends = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.send")
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[1]["params"]["prompt"], "EXPANDED STEER");
    assert_eq!(sends[1]["params"]["mode"], "immediate");
    assert_eq!(sends[2]["params"]["prompt"], "EXPANDED QUEUE");
    assert!(sends[2]["params"].get("mode").is_none());

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn unavailable_experimental_commands_make_skills_actionably_unavailable_without_text_fallback()
 {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        unavailable_command_catalog_arm(),
        send_arm("      event ordinary-idle session.idle '{}'\n"),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-incompatible";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("copilot"),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("list incompatible Copilot Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let SkillCatalogStatus::Unavailable { message } = &catalog.status else {
        panic!("incompatible command API makes Skills unavailable: {catalog:?}");
    };
    assert!(message.contains("experimental Session command interface"));
    assert!(message.contains("update the Copilot CLI"));
    assert_eq!(catalog.capabilities.max_distinct_invocations, Some(0));

    let rejected = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review".to_owned(),
                skill_invocations: vec![SkillInvocation {
                    skill_id: SkillId::new("copilot-forged"),
                    name: "review".to_owned(),
                    scope: None,
                    marker: SkillMarkerSpan { start: 0, end: 7 },
                }],
            },
        })
        .await;
    assert!(rejected.is_err(), "unavailable Skills cannot be admitted");
    assert!(
        copilot
            .requests()
            .iter()
            .all(|request| request["method"] != "session.send"),
        "failed listing never falls back to literal Skill text"
    );

    let ordinary = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue without a Skill".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("ordinary Copilot use remains available");
    settled_session(&client, ordinary.session.id, 0).await;
    let sends = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.send")
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["params"]["prompt"], "Continue without a Skill");

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_native_skill_invocation_fails_without_sending_literal_marker_text() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        command_catalog_arm(),
        rejected_command_invocation_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-invocation-failure";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    let skill = catalog.skills.first().expect("review Skill is offered");
    let invocation = SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        marker: SkillMarkerSpan { start: 0, end: 7 },
    };

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review this".to_owned(),
                skill_invocations: vec![invocation.clone()],
            },
        })
        .await
        .expect("admit Copilot Skill Prompt before the native race");
    let failed = settled_session(&client, created.session.id, 0).await;
    assert!(failed.activities.iter().any(|activity| {
        matches!(activity, Activity::Error { text, .. }
            if text.contains("fixture rejected native Skill invocation"))
    }));
    assert_eq!(failed.messages[0].content, "$review this");
    assert_eq!(failed.messages[0].skill_invocations, [invocation]);
    let serialized = serde_json::to_string(&failed).expect("serialize failed Session data");
    assert!(!serialized.contains("native-review"));
    assert!(!serialized.contains("/private/copilot"));
    assert!(!serialized.contains("private-plugin"));
    assert!(
        copilot
            .requests()
            .iter()
            .all(|request| request["method"] != "session.send"),
        "a rejected native invocation is never retried as plain text"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn copilot_rejects_two_distinct_skills_before_opening_or_invoking_a_user_session() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        two_skill_command_catalog_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-limit";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
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

    let rejected = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review $explain".to_owned(),
                skill_invocations: vec![
                    SkillInvocation {
                        skill_id: review.id.clone(),
                        name: review.name.clone(),
                        scope: review.scope.clone(),
                        marker: SkillMarkerSpan { start: 0, end: 7 },
                    },
                    SkillInvocation {
                        skill_id: explain.id.clone(),
                        name: explain.name.clone(),
                        scope: explain.scope.clone(),
                        marker: SkillMarkerSpan { start: 8, end: 16 },
                    },
                ],
            },
        })
        .await;
    assert!(
        rejected.is_err(),
        "Copilot admits at most one distinct Skill"
    );
    assert_eq!(
        copilot
            .requests()
            .iter()
            .filter(|request| request["method"] == "session.create")
            .count(),
        1,
        "only the catalog's temporary Session was opened"
    );
    assert!(
        copilot
            .requests()
            .iter()
            .all(|request| request["method"] != "session.commands.invoke"),
        "over-limit admission performs no partial native invocation"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn native_skill_changes_refresh_identity_and_nonsteer_commands_do_not_advertise_steer() {
    let changed = r#"      event skill-change commands.changed '{"commands":[{"name":"native-review","description":"changed"}]}'
      event change-idle session.idle '{}'
"#;
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        changing_nonsteer_skill_catalog_arms(),
        send_arm(changed),
        permission_decision_arm(),
        destroy_session_arm(),
        delete_session_arm(),
    ));
    let channel = "copilot-skill-change";
    let (_state_dir, workspace, server, mut client) = host_skills(&copilot, channel).await;
    let original = fresh_catalog(&mut client, workspace.path()).await;
    assert_eq!(
        original.capabilities.supported_deliveries,
        [SkillPromptDelivery::Initial, SkillPromptDelivery::Queue],
        "native commands unavailable during agent execution cannot be steered"
    );
    let original_id = original.skills[0].id.clone();

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Observe native Skill changes".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Copilot Session");
    settled_session(&client, created.session.id, 0).await;

    let mut update = next_skill_catalog(&mut client).await;
    if matches!(update.status, SkillCatalogStatus::Refreshing) {
        update = next_skill_catalog(&mut client).await;
    }
    assert!(matches!(
        update.status,
        SkillCatalogStatus::Fresh { warning: None }
    ));
    assert_ne!(
        update.skills[0].id, original_id,
        "a same-named Skill from another native path receives a new identity"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
