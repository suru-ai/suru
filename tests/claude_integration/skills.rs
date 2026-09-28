//! Claude-native Skill discovery and slash-command lowering.

use crate::{
    server_support::next_skill_catalog,
    support::{
        CLAUDE_MODELS, CLAUDE_SUGGESTED_VERSION, ScriptedClaude, agent_messages, hosting,
        hosting_runtime, list_models_arm, session_where, settled_session, user_turn_arm,
        version_arm,
    },
};
use serde_json::Value;
use suru::protocol::{
    Activity, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, MessageRole, PromptDelivery,
    PromptId, PromptStatus, ProviderId, SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor,
    SkillInvocation, SkillPromptDelivery, TextSpan, TurnStatus,
};

fn skill_catalog_arms() -> String {
    skill_catalog_arms_with_commands(
        r#"[{"name":"review","description":"Shadowed review (user)","argumentHint":""},{"name":"review","description":"Review the current change (project)","argumentHint":""},{"name":"explain","description":"Explain the current change (user)","argumentHint":"<topic>"},{"name":"charts","description":"(data-tools@official) Draw a chart","argumentHint":""},{"name":"invalid name","description":"Not invocable (project)","argumentHint":""},{"name":"anthropic-skills:pdf","description":"Work with PDF files (claude.ai sync)","argumentHint":""}]"#,
        r#"[{"name":"help","description":"Built-in help","argumentHint":"","builtin":true},{"name":"hidden","description":"A non-Skill command","argumentHint":"","builtin":true},{"name":"implement","description":"Implement a piece of work (user)","argumentHint":""}]"#,
    )
}

fn skill_catalog_arms_with(skills: &str) -> String {
    skill_catalog_arms_with_commands(
        skills,
        r#"[{"name":"help","description":"Built-in help","argumentHint":""},{"name":"hidden","description":"A non-Skill command","argumentHint":""}]"#,
    )
}

fn skill_catalog_arms_with_commands(skills: &str, commands: &str) -> String {
    format!(
        r#"{}    *'"subtype":"initialize"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"commands":__COMMANDS__,"agents":[],"output_style":"default","account":{{"email":"fixture@example.com","apiProvider":"firstParty"}}}}}}}}'
      ;;
{}    *'"subtype":"reload_skills"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"skills":__SKILLS__}}}}}}'
      ;;
"#,
        version_arm(CLAUDE_SUGGESTED_VERSION),
        list_models_arm(CLAUDE_MODELS),
    )
    .replace("__COMMANDS__", commands)
    .replace("__SKILLS__", skills)
}

fn invocation(skill: &SkillDescriptor, start: u32, end: u32) -> SkillInvocation {
    SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        span: TextSpan { start, end },
    }
}

async fn fresh_catalog(
    client: &mut suru::managed_client::ManagedClient,
    workspace: &std::path::Path,
) -> suru::protocol::SkillCatalog {
    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("claude"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
        })
        .await
        .expect("list Claude Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    next_skill_catalog(client).await
}

#[tokio::test]
async fn claude_discovers_native_skills_in_configured_short_lived_processes() {
    let claude = ScriptedClaude::new(&skill_catalog_arms());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let (server, mut client) = hosting(&claude, "claude-skill-catalog", state_dir.path()).await;

    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("claude"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("list Claude Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;

    assert!(matches!(
        catalog.status,
        SkillCatalogStatus::Fresh {
            warning: Some(ref warning)
        } if warning.contains("1 invalid")
    ));
    assert_eq!(
        catalog
            .skills
            .iter()
            .map(|skill| (
                skill.name.as_str(),
                skill.description.as_str(),
                skill.scope.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            (
                "anthropic-skills:pdf",
                "Work with PDF files",
                Some("claude.ai")
            ),
            (
                "charts",
                "Draw a chart",
                Some("Plugin · data-tools@official")
            ),
            ("explain", "Explain the current change", Some("User")),
            ("implement", "Implement a piece of work", Some("User")),
            ("review", "Review the current change", Some("Workspace")),
        ],
        "user-only and model-visible native Skills reach the Catalog, with safe scope metadata"
    );
    assert!(
        catalog.skills.iter().all(
            |skill| skill.id.as_str().starts_with("claude-") && skill.id.as_str() != skill.name
        ),
        "native slash names remain behind opaque identities"
    );
    assert_eq!(catalog.capabilities.max_distinct_invocations, Some(6));
    assert_eq!(
        catalog.capabilities.supported_deliveries,
        [SkillPromptDelivery::Initial, SkillPromptDelivery::Queue]
    );

    let discovery = claude
        .exact_launches()
        .into_iter()
        .find(|launch| {
            launch.working_directory == canonical_workspace
                && claude
                    .control_subtypes()
                    .contains(&"reload_skills".to_owned())
        })
        .expect("Skill discovery runs in the requested Workspace");
    assert_eq!(discovery.value("--setting-sources"), "user,project");
    assert!(
        !discovery.carries("--strict-mcp-config"),
        "Skill discovery keeps Claude's configured MCP servers"
    );

    let serialized = serde_json::to_string(&catalog).expect("serialize safe Claude Skill Catalog");
    assert!(!serialized.contains("argumentHint"));
    assert!(!serialized.contains("reload_skills"));

    drop(client);
    server.shutdown().await.expect("shut down server");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn claude_rejects_seven_distinct_skills_before_starting_native_input() {
    let native_skills = (1..=7)
        .map(|index| {
            serde_json::json!({
                "name": format!("skill-{index}"),
                "description": format!("Fixture Skill {index} (project)"),
                "argumentHint": "",
            })
        })
        .collect::<Vec<_>>();
    let skills_json = serde_json::to_string(&native_skills).expect("serialize native Skills");
    let claude = ScriptedClaude::new(&skill_catalog_arms_with(&skills_json));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client) = hosting(&claude, "claude-six-skill-limit", state_dir.path()).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    assert_eq!(catalog.skills.len(), 7, "the fixture offers enough Skills");

    let mut text = String::new();
    let mut invocations = Vec::new();
    for skill in &catalog.skills {
        if !text.is_empty() {
            text.push(' ');
        }
        let start = text.len() as u32;
        text.push('$');
        text.push_str(&skill.name);
        invocations.push(invocation(skill, start, text.len() as u32));
    }

    let rejected = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text,
                skill_invocations: invocations,
                attachments: Vec::new(),
            },
        })
        .await
        .expect_err("a seventh distinct Claude Skill is rejected");
    assert!(
        rejected.to_string().contains("more distinct Skills"),
        "the limit rejection is actionable: {rejected:#}"
    );
    assert!(
        claude
            .requests()
            .iter()
            .all(|request| request.get("type").and_then(Value::as_str) != Some("user")),
        "limit enforcement happens before any native Prompt input"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn claude_delivers_ordered_distinct_skills_for_initial_and_queued_prompts() {
    let timeline = r#"      prompt_count=$(( ${prompt_count:-0} + 1 ))
      if [ "$prompt_count" -eq 1 ]; then
        (
          while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
          emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":2,"num_turns":1,"result":"first","session_id":"prov-session"}'
        ) &
      else
        emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"num_turns":1,"result":"second","session_id":"prov-session"}'
      fi
"#;
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        skill_catalog_arms(),
        user_turn_arm(timeline)
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client) = hosting(&claude, "claude-skill-delivery", state_dir.path()).await;
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
    let initial_text = "$review $explain $review\nDo this";
    let initial_invocations = vec![
        invocation(review, 0, 7),
        invocation(explain, 8, 16),
        invocation(review, 17, 24),
    ];

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: initial_text.to_owned(),
                skill_invocations: initial_invocations.clone(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Claude Skill Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Claude Skill Session");
    session_where(
        &client,
        &mut feed,
        created.session.id,
        "the first Skill Turn starts",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Active)
        },
    )
    .await;

    let queued = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "$review".to_owned(),
                    skill_invocations: vec![invocation(review, 0, 7)],
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue a Skill-only Claude Prompt");
    assert_eq!(queued.status, PromptStatus::Pending);

    claude.release();
    let settled = settled_session(&client, created.session.id, 1).await;
    assert_eq!(settled.turns.len(), 2);
    assert!(
        settled
            .turns
            .iter()
            .all(|turn| turn.status == TurnStatus::Completed)
    );
    assert_eq!(
        settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| (
                message.content.as_str(),
                message.skill_invocations.as_slice()
            ))
            .collect::<Vec<_>>(),
        [
            (initial_text, initial_invocations.as_slice()),
            ("$review", queued.skill_invocations.as_slice()),
        ],
        "the Transcript preserves the visible Prompts and safe Skill records"
    );

    let native_prompts = claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|request| {
            request
                .pointer("/message/content/0/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        native_prompts,
        ["/review\n/explain\n  \nDo this", "/review"],
        "Claude receives distinct leading native commands in first-appearance order, followed by the full marker-free Prompt"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
    claude.wait_for_exits(3).await;
}

#[tokio::test]
async fn claude_reports_native_skill_rejection_without_plain_text_retry() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        skill_catalog_arms(),
        user_turn_arm(
            r#"      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":1,"num_turns":1,"errors":["fixture rejected native Skill invocation"],"session_id":"prov-session"}'
"#,
        )
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client) =
        hosting(&claude, "claude-native-skill-rejection", state_dir.path()).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered");
    let invocation = invocation(review, 0, 7);

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review".to_owned(),
                skill_invocations: vec![invocation.clone()],
                attachments: Vec::new(),
            },
        })
        .await
        .expect("admit Claude Skill Prompt");
    let failed = settled_session(&client, created.session.id, 0).await;

    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.messages[0].content, "$review");
    assert_eq!(failed.messages[0].skill_invocations, [invocation]);
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. }
            if text.contains("fixture rejected native Skill invocation")
    )));
    let serialized = serde_json::to_string(&failed).expect("serialize failed Session data");
    assert!(!serialized.contains("argumentHint"));
    assert!(!serialized.contains("reload_skills"));
    let native_prompts = claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .collect::<Vec<_>>();
    assert_eq!(
        native_prompts.len(),
        1,
        "Claude never retries as plain text"
    );
    assert_eq!(
        native_prompts[0]
            .pointer("/message/content/0/text")
            .and_then(Value::as_str),
        Some("/review")
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    claude.wait_for_exits(2).await;
}

#[tokio::test]
async fn claude_rejects_skill_steers_atomically_with_queue_guidance() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        skill_catalog_arms(),
        user_turn_arm(
            r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Working"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#,
        )
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client) = hosting(&claude, "claude-skill-steer", state_dir.path()).await;
    let catalog = fresh_catalog(&mut client, workspace.path()).await;
    let review = catalog
        .skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review Skill is offered");

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create active Claude Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to active Claude Session");
    session_where(
        &client,
        &mut feed,
        created.session.id,
        // Active is published before the CLI necessarily receives the Prompt.
        // A streamed reply proves its request log is ready for the assertion below.
        "Claude receives the plain Prompt and starts replying",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Active)
                && agent_messages(snapshot)
                    .iter()
                    .any(|message| message.content == "Working")
        },
    )
    .await;

    let rejected = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "$review improve this".to_owned(),
                    skill_invocations: vec![invocation(review, 0, 7)],
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect_err("Claude Skill-bearing Steer Prompt is rejected");
    assert!(
        rejected.to_string().contains("queue this Prompt instead"),
        "the rejection explains the supported alternative: {rejected:#}"
    );

    let unchanged = client
        .read_session(created.session.id)
        .await
        .expect("read unchanged Claude Session");
    assert_eq!(
        unchanged.prompts.len(),
        1,
        "the rejected Prompt is not admitted"
    );
    assert_eq!(
        claude
            .requests()
            .iter()
            .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
            .count(),
        1,
        "the rejected Skill steer sends no native input"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn claude_skill_discovery_survives_a_cli_that_lingers_after_its_stdin_closes() {
    // A CLI started in a directory it has never seen can keep working past the closed pipe.
    // Forcing it down is cleanup; the catalog it already answered with is what the user asked for.
    let claude = ScriptedClaude::new(&format!(
        r#"{}{}    *'"subtype":"initialize"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"commands":[],"agents":[],"output_style":"default","account":{{"email":"fixture@example.com","apiProvider":"firstParty"}}}}}}}}'
      ;;
    *'"subtype":"reload_skills"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"skills":[{{"name":"review","description":"Review the current change (project)","argumentHint":""}}]}}}}}}'
      sleep 30
      ;;
"#,
        version_arm(CLAUDE_SUGGESTED_VERSION),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let runtime = suru::provider::ClaudeRuntime::new(claude.executable())
        .with_process_exit_grace(std::time::Duration::from_millis(50));
    let (server, mut client) = hosting_runtime(
        runtime,
        "claude-lingering-skill-discovery",
        state_dir.path(),
    )
    .await;

    let catalog = fresh_catalog(&mut client, workspace.path()).await;

    assert!(
        matches!(catalog.status, SkillCatalogStatus::Fresh { warning: None }),
        "a CLI forced down after answering still yields a fresh Catalog, got {:?}",
        catalog.status
    );
    assert_eq!(
        catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["review"]
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
