//! The Broker: Suru's own Tools, served as a streamable-HTTP MCP endpoint on
//! the Server's loopback listener and reached with the bearer token a Session's
//! Provider start request carried (ADR 0034).
//!
//! Each test acts as the MCP client a Provider harness is — posting JSON-RPC to
//! the endpoint the controlled double was handed — and asserts only on what
//! that client and the doubles observe. The doubles are hosted under the real
//! Provider identities, because Enablement is a Setting keyed by them.

mod stops;

use std::{path::Path, sync::Arc};

use reqwest::StatusCode;
use serde_json::{Value, json};
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
        ApprovalPosture, ContextFill, CreateSessionRequest, Delegator, InitialPrompt, MessageRole,
        MessageStatus, ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionRole, ModelOptionSelection, ModelOptionValue, PromptDelivery, PromptId,
        ProviderId, ProviderUnavailability, RuntimeDescriptor, SessionId, SessionSnapshot,
        SessionStatus, SettingMutation, SubagentTreeChange, TranscriptItem, TurnStatus, Usage,
        UsageTotal,
    },
    provider::{
        BrokerHandoff, ContextFillReport, ProviderErrand, ProviderEvent, ProviderSubagentId,
        ProviderSubagentStatus,
    },
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

use crate::{
    provider_support::{
        ControlledProvider, ControlledProviderRuntime, ControlledProviderSession, StartRequest,
    },
    server_support::{
        PROGRESS_DEADLINE,
        broker::{MCP_PROTOCOL_VERSION, McpClient},
        read_runtime_descriptor,
    },
    subagent_tree::{changes_until, open_tree},
    support::{create_session, read_session, read_session_until},
};

mod deletion;
mod interventions;
mod late_output;
mod limits;
mod posture;
mod watches;

fn choice(id: &str, label: &str) -> ModelOptionChoice {
    ModelOptionChoice {
        id: ModelOptionChoiceId::new(id),
        label: label.to_owned(),
        description: None,
        availability: ModelAvailability::Available,
    }
}

fn select(
    id: &str,
    label: &str,
    role: ModelOptionRole,
    choices: Vec<ModelOptionChoice>,
    default: &str,
) -> ModelOptionDescriptor {
    ModelOptionDescriptor {
        id: ModelOptionId::new(id),
        label: label.to_owned(),
        description: None,
        role,
        kind: ModelOptionKind::Select {
            choices,
            default: ModelOptionChoiceId::new(default),
        },
    }
}

fn model(
    provider: &str,
    id: &str,
    name: &str,
    description: &str,
    is_default: bool,
    options: Vec<ModelOptionDescriptor>,
) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(id),
        display_name: name.to_owned(),
        description: description.to_owned(),
        is_default,
        availability: ModelAvailability::Available,
        options,
    }
}

/// Claude's catalog: a default Model with a select and a toggle Model Option,
/// and a second Model with none.
fn claude_models() -> Vec<ModelDescriptor> {
    vec![
        model(
            "claude",
            "opus",
            "Opus",
            "The most capable Claude",
            true,
            vec![
                select(
                    "effort",
                    "Effort",
                    ModelOptionRole::ReasoningEffort,
                    vec![
                        choice("low", "Low"),
                        choice("medium", "Medium"),
                        choice("high", "High"),
                    ],
                    "medium",
                ),
                ModelOptionDescriptor {
                    id: ModelOptionId::new("fast"),
                    label: "Fast mode".to_owned(),
                    description: None,
                    role: ModelOptionRole::Speed,
                    kind: ModelOptionKind::Toggle { default: false },
                },
            ],
        ),
        model(
            "claude",
            "haiku",
            "Haiku",
            "Quick and light",
            false,
            Vec::new(),
        ),
    ]
}

fn codex_models() -> Vec<ModelDescriptor> {
    vec![model(
        "codex",
        "gpt-5.5",
        "GPT-5.5",
        "Frontier coding model",
        true,
        vec![select(
            "reasoning_effort",
            "Reasoning effort",
            ModelOptionRole::ReasoningEffort,
            vec![
                choice("low", "Low"),
                choice("medium", "Medium"),
                choice("high", "High"),
                choice("xhigh", "Extra high"),
            ],
            "high",
        )],
    )]
}

/// What `list_providers` says of Claude's catalog above: every Model with its
/// Model Options, the choices each offers, their defaults, and which Model is
/// the Provider's default.
fn listed_claude() -> Value {
    json!({
        "id": "claude",
        "name": "claude",
        "enabled": true,
        "available": true,
        "models": [
            {
                "id": "opus",
                "name": "Opus",
                "description": "The most capable Claude",
                "default": true,
                "options": [
                    {
                        "id": "effort",
                        "name": "Effort",
                        "type": "select",
                        "choices": [
                            { "id": "low", "name": "Low" },
                            { "id": "medium", "name": "Medium" },
                            { "id": "high", "name": "High" },
                        ],
                        "default": "medium",
                    },
                    { "id": "fast", "name": "Fast mode", "type": "toggle", "default": false },
                ],
            },
            {
                "id": "haiku",
                "name": "Haiku",
                "description": "Quick and light",
                "default": false,
                "options": [],
            },
        ],
    })
}

fn listed_codex() -> Value {
    json!({
        "id": "codex",
        "name": "codex",
        "enabled": true,
        "available": true,
        "models": [
            {
                "id": "gpt-5.5",
                "name": "GPT-5.5",
                "description": "Frontier coding model",
                "default": true,
                "options": [
                    {
                        "id": "reasoning_effort",
                        "name": "Reasoning effort",
                        "type": "select",
                        "choices": [
                            { "id": "low", "name": "Low" },
                            { "id": "medium", "name": "Medium" },
                            { "id": "high", "name": "High" },
                            { "id": "xhigh", "name": "Extra high" },
                        ],
                        "default": "high",
                    },
                ],
            },
        ],
    })
}

/// A Server hosting Claude and Codex doubles that serve the catalogs above and
/// a Copilot double the user has yet to sign in to.
struct HostedProviders {
    server: RunningServer,
    claude: ControlledProvider,
    codex: ControlledProvider,
    runtimes: [Arc<ControlledProviderRuntime>; 3],
    workspace: tempfile::TempDir,
}

async fn host_providers(
    state_dir: &Path,
    channel: &str,
    config_dir: Option<&Path>,
) -> HostedProviders {
    let (claude_runtime, claude) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let (codex_runtime, codex) =
        ControlledProvider::with_provider(ProviderId::new("codex"), codex_models());
    let (copilot_runtime, _copilot) = ControlledProvider::with_provider(
        ProviderId::new("copilot"),
        vec![model("copilot", "gpt-4.1", "GPT-4.1", "", true, Vec::new())],
    );
    copilot_runtime.set_unavailable(Some(ProviderUnavailability::NotSignedIn));
    let mut config = ServerConfig::new(state_dir, channel).expect("configure server");
    if let Some(config_dir) = config_dir {
        config = config.with_config_dir(config_dir);
    }
    let server = server::spawn_with_providers(
        config,
        vec![
            claude_runtime.clone(),
            codex_runtime.clone(),
            copilot_runtime.clone(),
        ],
    )
    .await
    .expect("spawn server");
    HostedProviders {
        server,
        claude,
        codex,
        runtimes: [claude_runtime, codex_runtime, copilot_runtime],
        workspace: tempfile::tempdir().expect("create valid Workspace"),
    }
}

/// A Server hosting only the Claude double, for tests about one Session. It
/// has a config root of its own so a test can change a Setting as a client
/// does.
async fn host_claude(
    state_dir: &Path,
    config_dir: &Path,
    channel: &str,
) -> (RunningServer, ControlledProvider) {
    let (runtime, claude) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir, channel)
            .expect("configure server")
            .with_config_dir(config_dir),
        runtime,
    )
    .await
    .expect("spawn server");
    (server, claude)
}

/// The Agent Selection a Session on `models`' default Model begins with.
fn default_selection(models: &[ModelDescriptor]) -> AgentSelection {
    models
        .iter()
        .find(|model| model.is_default)
        .expect("the catalog has a default Model")
        .default_agent_selection()
}

fn session_request(
    workspace: &Path,
    selection: AgentSelection,
    text: &str,
) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: None,
        agent_selection: Some(selection),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

/// A Session on `selection`'s Provider whose first Turn is running, and the
/// Broker handoff its Provider start carried.
async fn start_session(
    descriptor: &RuntimeDescriptor,
    provider: &mut ControlledProvider,
    workspace: &Path,
    selection: AgentSelection,
) -> (SessionId, BrokerHandoff, ControlledProviderSession) {
    let created = create_session(
        descriptor,
        &session_request(workspace, selection.clone(), "Plan the work"),
    )
    .await;
    let start = next_start(provider).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a Provider start carries the Broker handoff beside its posture");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new(format!("{}-agent", selection.provider)),
        selection,
    });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    (created.session.id, handoff, provider_session)
}

async fn next_start(provider: &mut ControlledProvider) -> StartRequest {
    timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the Provider is asked to start")
}

async fn admit_prompt(descriptor: &RuntimeDescriptor, session_id: SessionId, text: &str) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt")
        .error_for_status()
        .expect("Prompt admission succeeds");
}

async fn mutate_setting(descriptor: &RuntimeDescriptor, mutation: SettingMutation) {
    reqwest::Client::new()
        .post(format!("{}/v1/settings", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&mutation)
        .send()
        .await
        .expect("mutate a Setting")
        .error_for_status()
        .expect("the mutation is accepted");
}

fn model_discoveries(hosted: &HostedProviders) -> [usize; 3] {
    hosted
        .runtimes
        .each_ref()
        .map(|runtime| runtime.model_discoveries())
}

#[tokio::test]
async fn a_session_lists_every_hosted_provider_through_the_broker_with_the_token_its_start_carried()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "broker-list-providers", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();

    let (_claude_session, claude_handoff, _claude_provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;
    assert!(
        claude_handoff
            .endpoint()
            .as_str()
            .starts_with(&format!("{}/", descriptor.base_url)),
        "the Broker is served on the Server's own loopback listener: {}",
        claude_handoff.endpoint()
    );
    let (_codex_session, codex_handoff, _codex_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    assert_eq!(codex_handoff.endpoint(), claude_handoff.endpoint());
    assert_ne!(
        codex_handoff.token(),
        claude_handoff.token(),
        "each Session is handed a token of its own"
    );

    let mut client = McpClient::handed(&claude_handoff);
    let initialized = client.initialize().await;
    assert_eq!(initialized["protocolVersion"], json!(MCP_PROTOCOL_VERSION));
    assert_eq!(initialized["serverInfo"]["name"], json!("suru"));
    assert!(
        initialized["capabilities"]["tools"].is_object(),
        "the Broker offers Tools: {initialized}"
    );

    let tools = client.request("tools/list", json!({})).await;
    let tools = tools["tools"].as_array().expect("tools/list lists Tools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("every Tool is named"))
            .collect::<Vec<_>>(),
        [
            "list_providers",
            "spawn_subagent",
            "read_subagent",
            "stop_subagent"
        ],
        "the Broker offers the Tools it has so far, in its own order"
    );
    for (tool, answers_with, read_only) in [
        (&tools[0], "\"providers\"", true),
        (&tools[1], "\"session_id\"", false),
        (&tools[2], "\"status\"", true),
        (&tools[3], "\"stopped\"", false),
    ] {
        assert_eq!(tool["inputSchema"]["type"], json!("object"));
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|description| description.contains(answers_with)),
            "each Tool's description documents the shape it answers with: {tool}"
        );
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(read_only));
    }
    assert_eq!(
        tools[1]["inputSchema"]["required"],
        json!(["provider", "model", "name", "description", "prompt"]),
        "a spawn names its target and its Delegation, and may leave its Model Options out"
    );
    assert_eq!(
        tools[2]["inputSchema"]["required"],
        json!(["id"]),
        "a stop names the Subagent it stops"
    );

    let listing = client.list_providers().await;
    assert_eq!(
        listing,
        json!({
            "providers": [
                listed_claude(),
                listed_codex(),
                {
                    "id": "copilot",
                    "name": "copilot",
                    "enabled": true,
                    "available": false,
                    "reason": "not_signed_in",
                    "detail": "the copilot CLI is not signed in",
                    "models": [],
                },
            ],
        }),
        "every hosted Provider is listed in the hosted order; the unavailable one \
         keeps its place with its reason and offers no Model"
    );

    let refused = client
        .call_tool("list_providers", json!({ "provider": "codex" }))
        .await;
    assert_eq!(
        refused["isError"],
        json!(true),
        "an argument list_providers does not take is refused as the Tool's own error"
    );
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("takes no arguments")),
        "in words the Agent reads: {refused}"
    );

    // Availability is reported as Suru last found it: a second call, from the
    // other Session, asks no Provider for its Models again.
    let asked = model_discoveries(&hosted);
    let mut codex_client = McpClient::handed(&codex_handoff);
    codex_client.initialize().await;
    assert_eq!(codex_client.list_providers().await, listing);
    assert_eq!(
        model_discoveries(&hosted),
        asked,
        "list_providers reports Availability without re-probing any Provider"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_disabled_provider_is_listed_as_turned_off_and_never_asked_for_its_models() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        "{ \"provider\": { \"codex\": { \"enabled\": false } } }\n",
    )
    .expect("write Config Document");
    let mut hosted = host_providers(
        state_dir.path(),
        "broker-disabled-provider",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_session, handoff, _provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;

    let mut client = McpClient::handed(&handoff);
    client.initialize().await;
    let listing = client.list_providers().await;
    assert_eq!(
        listing["providers"][1],
        json!({
            "id": "codex",
            "name": "codex",
            "enabled": false,
            "available": null,
            "models": [],
        }),
        "a Provider the user turned off is listed as their choice, with no \
         Availability because Suru never looked"
    );
    assert_eq!(listing["providers"][0], listed_claude());
    assert_eq!(
        hosted.runtimes[1].model_discoveries(),
        0,
        "a disabled Provider is never asked for its Models"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_token_the_server_never_minted_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "broker-unknown-token").await;
    let descriptor = server.descriptor().clone();
    let (_session, handoff, _provider) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;
    let endpoint = handoff.endpoint().as_str();

    for (authorization, refused) in [
        (None, "a request presenting no token"),
        (
            Some("Bearer not-a-minted-token".to_owned()),
            "a token never minted",
        ),
        (
            Some(format!("Bearer {}", descriptor.token)),
            "the Server's own API token, which names no Session",
        ),
        (
            Some(handoff.token().secret().to_owned()),
            "a Session's token presented without the Bearer scheme",
        ),
    ] {
        assert_eq!(
            McpClient::presenting(endpoint, authorization)
                .initialize_status()
                .await,
            StatusCode::UNAUTHORIZED,
            "the Broker refuses {refused}"
        );
    }
    assert_eq!(
        McpClient::handed(&handoff).initialize_status().await,
        StatusCode::OK,
        "while the Session's own token is answered"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sessions_token_is_retired_when_its_provider_closes_and_reminted_at_relaunch() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "broker-token-retirement",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (session_id, first, provider_session) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;
    provider_session.emit(ProviderEvent::TurnCompleted);
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        session_id,
        "the first Turn completes",
        |snapshot| {
            snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
        },
    )
    .await;
    assert_eq!(
        McpClient::handed(&first).initialize_status().await,
        StatusCode::OK
    );

    // The Provider process ends: its event stream closes under the Server.
    drop(provider_session);
    timeout(PROGRESS_DEADLINE, async {
        while McpClient::handed(&first).initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the token is retired once the Provider it was handed to has closed");

    admit_prompt(&descriptor, session_id, "Carry on").await;
    let relaunch = next_start(&mut claude).await;
    let second = relaunch
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker too");
    assert_eq!(second.endpoint(), first.endpoint());
    assert_ne!(
        second.token(),
        first.token(),
        "a relaunch is handed a freshly minted token"
    );
    let mut relaunched = relaunch.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the queued Prompt's Turn reaches the relaunched Provider")
        .succeed();
    assert_eq!(
        McpClient::handed(&second).initialize_status().await,
        StatusCode::OK
    );
    assert_eq!(
        McpClient::handed(&first).initialize_status().await,
        StatusCode::UNAUTHORIZED,
        "and the retired token stays refused"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn with_the_broker_off_a_provider_start_carries_no_handoff_and_the_endpoint_answers_nothing()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "broker-turned-off").await;
    let descriptor = server.descriptor().clone();
    let (_session, handoff, _provider) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;
    assert_eq!(
        McpClient::handed(&handoff).initialize_status().await,
        StatusCode::OK
    );

    mutate_setting(
        &descriptor,
        SettingMutation::BrokerEnabled { value: Some(false) },
    )
    .await;
    assert_eq!(
        McpClient::handed(&handoff).initialize_status().await,
        StatusCode::NOT_FOUND,
        "a Provider already running keeps its handoff, but the endpoint no longer answers"
    );
    assert_eq!(
        McpClient::presenting(handoff.endpoint().as_str(), None)
            .initialize_status()
            .await,
        StatusCode::NOT_FOUND,
        "nor does it answer anyone else"
    );

    create_session(
        &descriptor,
        &session_request(
            workspace.path(),
            default_selection(&claude_models()),
            "Start fresh",
        ),
    )
    .await;
    let start = next_start(&mut claude).await;
    assert_eq!(
        start.broker(),
        None,
        "a Provider started while the Broker is off is handed no endpoint"
    );
    assert!(
        start.approval_posture().is_some(),
        "though it is handed its posture as ever"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_is_handed_no_broker_while_its_sessions_provider_start_is() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "broker-errand").await;
    let descriptor = server.descriptor().clone();

    // Admitting a Session's first Prompt asks for its Title through an Errand
    // on the Session's own Provider, beside that Provider's start.
    create_session(
        &descriptor,
        &session_request(
            workspace.path(),
            default_selection(&claude_models()),
            "Name this work",
        ),
    )
    .await;
    let start = next_start(&mut claude).await;
    assert!(start.broker().is_some());
    let errand = timeout(PROGRESS_DEADLINE, claude.next_errand())
        .await
        .expect("the Title Errand reaches the Provider");
    // An Errand carries one Prompt, the shape of its answer, the Selection it
    // runs under and where it runs — no Tools, so no Broker. Naming every
    // field here means a handoff added to Errands fails to build until this
    // test is changed to accept it.
    let ProviderErrand {
        prompt: _,
        schema: _,
        selection: _,
        execution_directory: _,
    } = errand.errand();

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn nothing_about_the_broker_is_written_into_the_runtime_descriptor() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config =
        ServerConfig::new(state_dir.path(), "broker-runtime-descriptor").expect("configure server");
    let descriptor_path = config.descriptor_path();
    let (runtime, mut claude) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let server = server::spawn_with_provider(config, runtime)
        .await
        .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let (_session, handoff, _provider) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;

    let written = std::fs::read_to_string(&descriptor_path).expect("read the runtime descriptor");
    assert!(
        !written.contains(handoff.token().secret()),
        "no Broker token is written where any local process can read it"
    );
    assert!(
        !written.contains("broker"),
        "nor anything naming the Broker: {written}"
    );
    let mut fields = serde_json::from_str::<serde_json::Map<String, Value>>(&written)
        .expect("the runtime descriptor is a JSON object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    assert_eq!(
        fields,
        [
            "base_url",
            "build_identity",
            "instance_id",
            "pid",
            "protocol_version",
            "token"
        ]
    );
    assert_eq!(read_runtime_descriptor(&descriptor_path), descriptor);

    server.shutdown().await.expect("shut down server");
}

/// What every spawn below asks its Subagent to do.
const DELEGATION: &str =
    "Find every seam where a Provider plugs into Suru, and say which file holds each.";

/// The Delegation as the Subagent's Provider receives it: one leading line
/// naming the delegating Agent, then what it asked.
fn delegated_by(delegator: &str) -> String {
    format!("Delegated to you through Suru by {delegator}.\n\n{DELEGATION}")
}

/// `spawn_subagent`'s arguments for a Subagent named Researcher on
/// `provider`'s `model`, with `options` as the Agent sent them.
fn researcher(provider: &str, model: &str, options: Value) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "options": options,
        "name": "Researcher",
        "description": "Survey the Provider seams",
        "prompt": DELEGATION,
    })
}

/// The Agent Selection a Codex Subagent on GPT-5.5 at `effort` runs under.
fn codex_selection(effort: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5.5"),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(effort),
            },
        }],
    }
}

/// A Claude Session whose first Turn is working, on a Server hosting it beside
/// Codex and an unavailable Copilot, and the MCP client its Agent is, holding
/// the Broker token its Provider start carried.
struct Delegating {
    hosted: HostedProviders,
    descriptor: RuntimeDescriptor,
    caller: SessionId,
    caller_provider: ControlledProviderSession,
    handoff: BrokerHandoff,
    client: McpClient,
}

async fn delegating(state_dir: &Path, channel: &str, config_dir: Option<&Path>) -> Delegating {
    let mut hosted = host_providers(state_dir, channel, config_dir).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (caller, handoff, caller_provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;
    Delegating {
        hosted,
        descriptor,
        caller,
        caller_provider,
        handoff,
        client,
    }
}

/// The brokered Subagent's own Provider, started on `provider`'s double as
/// the Agent its start answers with, and the first Turn it is asked to run,
/// taken up.
async fn run_child(
    provider: &mut ControlledProvider,
    selection: AgentSelection,
) -> (ControlledProviderSession, String) {
    let start = next_start(provider).await;
    let mut child = start.succeed(AgentIdentity {
        agent: AgentId::new(format!("{}-agent", selection.provider)),
        selection,
    });
    let turn = timeout(PROGRESS_DEADLINE, child.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider");
    let delivered = turn.prompt().to_owned();
    turn.succeed();
    (child, delivered)
}

/// A brokered Subagent on Codex spawned by the Session `delegating` holds,
/// with its first Turn working on its own Provider.
async fn spawn_working_child(
    delegating: &mut Delegating,
) -> (SessionId, ControlledProviderSession) {
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    (child_id, child_provider)
}

/// The Subagent row `snapshot` holds for `child`.
fn row_for(snapshot: &SessionSnapshot, child: SessionId) -> &Activity {
    snapshot
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child))
        .unwrap_or_else(|| panic!("the Transcript holds a row for {child}"))
}

fn row_status(snapshot: &SessionSnapshot, child: SessionId) -> (ActivityStatus, Option<u64>) {
    let Activity::Subagent {
        status,
        duration_ms,
        ..
    } = row_for(snapshot, child)
    else {
        unreachable!()
    };
    (*status, *duration_ms)
}

fn row_model(snapshot: &SessionSnapshot, child: SessionId) -> Option<ModelId> {
    let Activity::Subagent { model, .. } = row_for(snapshot, child) else {
        unreachable!()
    };
    model.clone()
}

async fn read_until(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    described: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    read_session_until(
        &reqwest::Client::new(),
        descriptor,
        session_id,
        described,
        predicate,
    )
    .await
}

#[tokio::test]
async fn a_spawned_brokered_subagent_is_a_child_session_on_the_agent_selection_it_named() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-child", None).await;
    let workspace = delegating.hosted.workspace.path().to_owned();

    let child_id = delegating
        .client
        .spawn_subagent(researcher(
            "codex",
            "gpt-5.5",
            json!({ "reasoning_effort": "low" }),
        ))
        .await;

    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(
        child.session.parent,
        Some(delegating.caller),
        "the Subagent's Session is a child of the Session whose Agent spawned it"
    );
    assert_eq!(
        child.session.agent_selection,
        Some(codex_selection("low")),
        "on the Provider, Model and Model Options the spawn named"
    );
    assert_eq!(child.title, "Survey the Provider seams");
    assert_eq!(child.session.execution_directory.path, workspace);

    let start = next_start(&mut delegating.hosted.codex).await;
    assert_eq!(
        start.execution_directory(),
        workspace,
        "the Subagent works in its caller's Execution Directory"
    );
    assert_eq!(
        start.resume_state(),
        None,
        "a new Subagent has nothing to resume"
    );
    assert_eq!(
        start.approval_posture().map(ApprovalPosture::provider),
        Some(ProviderId::new("codex")),
        "its own Provider's posture"
    );
    let child_handoff = start
        .broker()
        .cloned()
        .expect("the Subagent is handed the Broker too");
    assert_ne!(
        child_handoff.token(),
        delegating.handoff.token(),
        "on a token of its own"
    );
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    let turn = timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider");
    assert_eq!(
        turn.prompt(),
        delegated_by("the Agent working on \"Plan the work\""),
        "its first Turn's input is the Delegation, naming the Agent that delegated it by its \
         Session's Title"
    );
    assert_eq!(
        turn.selection(),
        &codex_selection("low"),
        "under the Agent Selection the spawn named"
    );
    turn.succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn the_callers_working_turn_gains_a_row_leading_into_a_child_that_opens_with_the_delegation()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-row", None).await;

    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;

    let caller = read_session(&delegating.descriptor, delegating.caller).await;
    let Activity::Subagent {
        turn_id,
        status,
        name,
        description,
        model,
        session_id,
        duration_ms,
        ..
    } = row_for(&caller, child_id)
    else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, caller.turns[0].id,
        "the row stands in the Turn whose Agent spawned it"
    );
    assert_eq!(caller.turns[0].status, TurnStatus::Active);
    assert_eq!(
        (name.as_str(), description.as_str()),
        ("Researcher", "Survey the Provider seams")
    );
    assert_eq!(*session_id, child_id, "and leads into the child's Session");
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(
        (model, duration_ms),
        (&None, &None),
        "no Model is known before the child's Provider confirms one, and no duration before it \
         settles"
    );

    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(child.turns.len(), 1);
    assert_eq!(
        child.turns[0].prompt_id, None,
        "a Delegation, not a Prompt, begins it"
    );
    let Some(TranscriptItem::Message { message_id }) = child.transcript.first() else {
        panic!("the child's Transcript opens with a Message: {child:?}");
    };
    let opening = child
        .messages
        .iter()
        .find(|message| message.id == *message_id)
        .expect("the opening Message is held");
    assert_eq!(
        opening.role,
        MessageRole::Delegation(Delegator {
            session_id: delegating.caller,
            name: None,
        }),
        "the Delegation stands as a Message from the Agent that delegated it, apart from a \
         user Message"
    );
    assert_eq!(opening.content, DELEGATION);
    assert_eq!(opening.turn_id, child.turns[0].id);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn the_caller_is_working_until_its_brokered_subagent_settles_and_the_row_settles_completed_with_it()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-working", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    // The caller's own Turn settles at its Provider's boundary while the
    // Subagent it spawned works on.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let waiting = read_until(
        &descriptor,
        delegating.caller,
        "the caller's Turn settles while its Subagent works",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert!(
        waiting.working_since().is_some(),
        "the caller reads as Working while its brokered Subagent works"
    );
    assert_eq!(
        row_status(&waiting, child_id),
        (ActivityStatus::Active, None)
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the row settles with the child's Turn",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    let (status, duration_ms) = row_status(&settled, child_id);
    assert_eq!(status, ActivityStatus::Completed);
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        duration_ms,
        child.turns[0]
            .settled_at
            .zip(child.turns[0].started_at)
            .map(|(settled, started)| settled.0 - started.0),
        "and says how long the Subagent worked, timed from its spawn"
    );
    wake_for_the_report(&mut delegating.caller_provider, child_id).await;
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "nothing below the caller works any more",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(idle.session.status, SessionStatus::Idle);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_whose_turn_fails_settles_its_row_failed_with_a_duration() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-failed", None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnFailed {
            message: "the sandbox refused the command".to_owned(),
        })
        .await;
    let settled = read_until(
        &delegating.descriptor,
        delegating.caller,
        "the row settles with the child's failed Turn",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    let (status, duration_ms) = row_status(&settled, child_id);
    assert_eq!(status, ActivityStatus::Failed);
    assert!(duration_ms.is_some(), "a failed stretch was timed too");
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert!(child.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text == "the sandbox refused the command"
    )));

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn the_model_a_brokered_subagents_provider_confirms_shows_on_its_row_and_in_the_tree() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-model", None).await;
    let descriptor = delegating.descriptor.clone();
    let (tree, mut updates) = open_tree(&descriptor, delegating.caller).await;
    let mut revision = tree.revision;

    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let spawned = changes_until(&mut updates, &mut revision, |change| {
        matches!(change, SubagentTreeChange::SubagentSpawned { .. })
    })
    .await;
    let Some(SubagentTreeChange::SubagentSpawned { entry }) = spawned.last() else {
        unreachable!()
    };
    assert_eq!(
        (entry.session_id, entry.parent_session_id),
        (child_id, delegating.caller)
    );
    assert_eq!(entry.name, "Researcher");
    assert_eq!(
        entry.model, None,
        "no Model is known until the child's Provider confirms one"
    );

    // The child's Provider starts up naming a Model of its own — the
    // default it reports before anything is asked of it — and then takes its
    // first Turn under the Model the spawn chose. Taking the Turn is its
    // first word on which Model runs the Subagent; its startup default says
    // nothing of that.
    let (child_provider, _) = run_child(
        &mut delegating.hosted.codex,
        AgentSelection {
            model: ModelId::new("gpt-5.4"),
            ..codex_selection("high")
        },
    )
    .await;
    let confirmed = changes_until(&mut updates, &mut revision, |change| {
        matches!(change, SubagentTreeChange::SubagentModelChanged { .. })
    })
    .await;
    assert_eq!(
        confirmed.last(),
        Some(&SubagentTreeChange::SubagentModelChanged {
            session_id: child_id,
            model: ModelId::new("gpt-5.5"),
        }),
        "the entry carries the Model the child's Provider took the Turn under, not the one its \
         startup named"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(row_model(&caller, child_id), Some(ModelId::new("gpt-5.5")));

    // Later the Provider reports running another.
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::AgentSelectionChanged {
            selection: AgentSelection {
                model: ModelId::new("gpt-5.3"),
                ..codex_selection("low")
            },
        })
        .await;
    let revised = changes_until(&mut updates, &mut revision, |change| {
        matches!(change, SubagentTreeChange::SubagentModelChanged { .. })
    })
    .await;
    assert_eq!(
        revised.last(),
        Some(&SubagentTreeChange::SubagentModelChanged {
            session_id: child_id,
            model: ModelId::new("gpt-5.3"),
        }),
        "the entry follows the Model the Provider confirmed last"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(row_model(&caller, child_id), Some(ModelId::new("gpt-5.3")));
    let (fresh, _updates) = open_tree(&descriptor, child_id).await;
    assert_eq!(
        fresh
            .subagents
            .iter()
            .find(|entry| entry.session_id == child_id)
            .and_then(|entry| entry.model.clone()),
        Some(ModelId::new("gpt-5.3")),
        "and a fresh snapshot of the tree, opened from the child, carries it"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagents_usage_is_its_own_and_rolls_into_its_callers_total() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-usage", None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    let measured = |fresh_input, output| Usage {
        fresh_input_tokens: Some(fresh_input),
        output_tokens: Some(output),
        ..Usage::default()
    };

    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(4_000, 1_000),
            cost: None,
        })
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(2_000, 500),
            cost: None,
        })
        .await;

    let tokens = |total: Option<UsageTotal>| {
        total.map(|total| (total.fresh_input_tokens, total.output_tokens))
    };
    let caller = read_until(
        &delegating.descriptor,
        delegating.caller,
        "the caller's total carries its Subagent's Usage",
        |snapshot| tokens(snapshot.total_usage()) == Some((Some(6_000), Some(1_500))),
    )
    .await;
    assert_eq!(
        tokens(caller.subagent_usage),
        Some((Some(2_000), Some(500))),
        "the roll-up stands apart from the caller's own Turns"
    );
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(
        tokens(child.total_usage()),
        Some((Some(2_000), Some(500))),
        "the Subagent's Session keeps its own Usage"
    );
    assert_eq!(child.turns[0].usage, Some(measured(2_000, 500)));

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_spawn_on_a_provider_or_model_that_cannot_be_chosen_is_refused_in_words_the_agent_reads()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        "{ \"provider\": { \"codex\": { \"enabled\": false } } }\n",
    )
    .expect("write Config Document");
    let mut delegating = delegating(
        state_dir.path(),
        "broker-spawn-refused",
        Some(config_dir.path()),
    )
    .await;

    for (arguments, says) in [
        (
            researcher("codex", "gpt-5.5", json!({})),
            "Provider `codex` is turned off in Suru",
        ),
        (
            researcher("copilot", "gpt-4.1", json!({})),
            "Provider `copilot` cannot be used now: the copilot CLI is not signed in",
        ),
        (
            researcher("gemini", "pro", json!({})),
            "Suru hosts no Provider `gemini`; the Providers it hosts are `claude`, `codex`, \
             `copilot`",
        ),
        (
            researcher("claude", "sonnet-9", json!({})),
            "Provider `claude` offers no Model `sonnet-9`; choose one of `opus`, `haiku`",
        ),
        (
            researcher("claude", "opus", json!({ "effort": "ultra" })),
            "Model Option `effort` has no choice `ultra`; choose one of `low`, `medium`, `high`",
        ),
        (
            researcher("claude", "opus", json!({ "fast": "yes" })),
            "Model Option `fast` is a toggle and takes true or false",
        ),
        (
            researcher("claude", "opus", json!({ "verbosity": "low" })),
            "Model `opus` has no Model Option `verbosity`; its Model Options are `effort`, `fast`",
        ),
        (
            json!({ "provider": "claude", "model": "opus", "name": "Researcher",
                    "description": "Survey the Provider seams", "prompt": "  " }),
            "`prompt` is empty",
        ),
        (
            json!({ "provider": "claude", "model": "opus", "name": "Researcher",
                    "prompt": DELEGATION }),
            "spawn_subagent needs `description`",
        ),
        (
            json!({ "provider": "claude", "model": "opus", "name": "Researcher",
                    "description": "Survey", "prompt": DELEGATION, "isolation": "worktree" }),
            "spawn_subagent takes no argument `isolation`",
        ),
    ] {
        let refusal = delegating.client.refusal("spawn_subagent", arguments).await;
        assert!(
            refusal.contains(says),
            "the refusal says {says:?}, but said {refusal:?}"
        );
    }

    let caller = read_session(&delegating.descriptor, delegating.caller).await;
    assert!(
        !caller
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Subagent { .. })),
        "a refused spawn adds no row"
    );
    assert!(
        delegating.hosted.codex.try_next_start().is_none()
            && delegating.hosted.claude.try_next_start().is_none(),
        "and starts no Provider"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn model_options_a_spawn_leaves_out_take_the_models_defaults() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-defaults", None).await;
    let caller = read_session(&delegating.descriptor, delegating.caller).await;

    let child_id = delegating
        .client
        .spawn_subagent(researcher("claude", "opus", json!({ "fast": true })))
        .await;

    let defaulted = AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("opus"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("medium"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("fast"),
                value: ModelOptionValue::Toggle { enabled: true },
            },
        ],
    };
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(
        child.session.agent_selection,
        Some(defaulted.clone()),
        "the option the spawn named is set, and the one it left out takes the Model's default"
    );

    // A Subagent on its spawner's own Provider runs a Provider of its own all
    // the same, under its spawner's very posture.
    let start = next_start(&mut delegating.hosted.claude).await;
    assert_eq!(
        start.approval_posture(),
        caller
            .session
            .approval_posture
            .as_ref()
            .map(|posture| &posture.value),
        "a brokered Subagent on its spawner's Provider acts under its spawner's posture"
    );
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider");
    assert_eq!(turn.selection(), &defaulted);
    turn.succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

/// The caller's Turn settled at its Provider's boundary, as `described`.
/// Takes up the Continuation the Subagent Report of `child` wakes an idle
/// caller's Provider into (ADR 0035), and settles it at that Provider's own
/// boundary, as its Agent answering the Report would.
async fn wake_for_the_report(caller: &mut ControlledProviderSession, child: SessionId) {
    let woken = timeout(PROGRESS_DEADLINE, caller.next_turn())
        .await
        .expect("the Subagent's Report wakes the idle caller's Provider");
    assert_eq!(
        woken
            .reports()
            .iter()
            .map(|report| report.subagent)
            .collect::<Vec<_>>(),
        [child],
        "a Continuation whose input is the Subagent's Report"
    );
    woken.succeed();
    caller
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
}

/// Takes up the steer the Subagent Report of `child` delivers into a caller
/// whose Turn still works (ADR 0035), as its Provider would.
async fn steered_by_the_report(caller: &mut ControlledProviderSession, child: SessionId) {
    let steer = timeout(PROGRESS_DEADLINE, caller.next_steer())
        .await
        .expect("the Subagent's Report steers the caller's working Turn");
    assert_eq!(
        steer
            .reports()
            .iter()
            .map(|report| report.subagent)
            .collect::<Vec<_>>(),
        [child],
        "a steer delivering the Subagent's Report"
    );
    steer.succeed();
}

async fn settle_callers_turn(delegating: &Delegating, described: &str) -> SessionSnapshot {
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &delegating.descriptor,
        delegating.caller,
        described,
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await
}

#[tokio::test]
async fn a_spawn_by_a_session_whose_turn_has_settled_opens_a_continuation_holding_the_row() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-continuation", None).await;
    let descriptor = delegating.descriptor.clone();
    let fill = ContextFill {
        occupied_tokens: 12_400,
        capacity_tokens: Some(200_000),
    };
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::ContextFill {
            report: ContextFillReport {
                turn_id: None,
                sequence: 1,
                fill,
            },
        })
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the caller's Context Fill is known",
        |snapshot| snapshot.session.context_fill == Some(fill),
    )
    .await;
    let idle = settle_callers_turn(&delegating, "the caller's Turn settles").await;
    assert_eq!(
        idle.working_since(),
        None,
        "nothing in the caller's tree works"
    );

    // Its Agent spawns all the same — from work its Provider carries on past
    // the Turn, say — and the spawn is taken rather than refused.
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;

    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        caller.turns.len(),
        2,
        "a Turn is begun to hold the row: {:?}",
        caller.turns
    );
    let continuation = &caller.turns[1];
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation: begun by no Prompt, and in a top-level Session by no Delegation"
    );
    assert_eq!(
        continuation.status,
        TurnStatus::Completed,
        "settled at once, since the caller's Provider knows nothing of it and will never settle it"
    );
    assert!(
        continuation.started_at.is_some() && continuation.settled_at.is_some(),
        "it records when it began and settled, as any Turn does"
    );
    assert_eq!(
        continuation.agent, caller.turns[0].agent,
        "held by the caller's own Agent"
    );
    assert_eq!(
        caller.session.context_fill,
        Some(fill),
        "so the caller's Context Fill stands, its Model unchanged"
    );
    let Activity::Subagent {
        turn_id, status, ..
    } = row_for(&caller, child_id)
    else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, continuation.id,
        "the row stands in the Continuation"
    );
    assert_eq!(
        *status,
        ActivityStatus::Active,
        "and works on past its settle, as the child does"
    );
    assert!(
        caller.working_since().is_some(),
        "the caller reads as Working through its Subagent"
    );
    assert_eq!(
        caller
            .activities
            .iter()
            .filter(|activity| matches!(activity, Activity::Subagent { .. }))
            .count(),
        1
    );

    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the row settles with the child's Turn, though the Turn holding it has settled",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    let (status, duration_ms) = row_status(&settled, child_id);
    assert_eq!(status, ActivityStatus::Completed);
    assert!(duration_ms.is_some());
    // The Subagent's Report wakes the caller's idle Agent into a Continuation
    // of its own, which its Provider settles at its own boundary (ADR 0035).
    wake_for_the_report(&mut delegating.caller_provider, child_id).await;
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "nothing below the caller works any more",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(idle.session.status, SessionStatus::Idle);

    // Nothing is left owing the caller's Provider a Turn: the next Prompt
    // begins one of its own there, as after any settled Turn.
    admit_prompt(
        &descriptor,
        delegating.caller,
        "What did the Researcher find?",
    )
    .await;
    let turn = timeout(PROGRESS_DEADLINE, delegating.caller_provider.next_turn())
        .await
        .expect("the next Prompt reaches the caller's Provider as a Turn of its own");
    assert_eq!(turn.prompt(), "What did the Researcher find?");
    turn.succeed();
    let prompted = read_until(
        &descriptor,
        delegating.caller,
        "the Prompt begins the caller's next Turn",
        |snapshot| snapshot.turns.len() == 4,
    )
    .await;
    assert!(prompted.turns[3].prompt_id.is_some());
    assert_eq!(prompted.turns[3].status, TurnStatus::Active);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_native_subagents_spawn_after_its_parents_turn_settled_opens_a_continuation_in_the_tokens_session()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-spawn-native-late", None).await;
    let descriptor = delegating.descriptor.clone();
    let native = ProviderSubagentId::new("task-1");
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: native.clone(),
            name: "Explore".to_owned(),
            description: "Map the seams".to_owned(),
            delegation: Some("Map every seam.".to_owned()),
        })
        .await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the native Subagent's row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Activity::Subagent {
        session_id: native_id,
        ..
    } = caller
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
        .expect("the native row")
    else {
        unreachable!()
    };
    let native_id = *native_id;
    let settled = settle_callers_turn(
        &delegating,
        "the caller's Turn settles while its native Subagent works on",
    )
    .await;
    assert!(settled.working_since().is_some());

    // The native Subagent shares its parent's Provider process, and with it
    // the token its parent's start carried: its spawn is the token's Session's.
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;

    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.session.parent, Some(delegating.caller));
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(caller.turns.len(), 2);
    let continuation = &caller.turns[1];
    assert_eq!(continuation.prompt_id, None);
    assert_eq!(continuation.status, TurnStatus::Completed);
    let Activity::Subagent { turn_id, .. } = row_for(&caller, child_id) else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, continuation.id,
        "the row stands in a Continuation of the token's Session"
    );
    let native_session = read_session(&descriptor, native_id).await;
    assert_eq!(
        native_session.turns.len(),
        1,
        "and the native Subagent's own Session gains no Turn"
    );

    // The caller stays Working until everything beneath it has settled.
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let brokered_settled = read_until(
        &descriptor,
        delegating.caller,
        "the brokered row settles",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;
    assert!(
        brokered_settled.working_since().is_some(),
        "the native Subagent works on"
    );
    wake_for_the_report(&mut delegating.caller_provider, child_id).await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: native,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "nothing below the caller works any more",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(idle.session.status, SessionStatus::Idle);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

/// A brokered Subagent working when the Server stops cleanly: the stop
/// settles its Turn, and its row with it, before the Server is gone.
async fn stop_with_a_brokered_subagent_working(
    state_dir: &Path,
    channel: &str,
    said: Option<&str>,
) -> (SessionId, SessionId) {
    let mut delegating = delegating(state_dir, channel, None).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    read_until(
        &delegating.descriptor,
        child_id,
        "the Subagent is working on its Model",
        |snapshot| snapshot.turns[0].agent.is_some(),
    )
    .await;
    if let Some(text) = said {
        say(&delegating.descriptor, child_id, &child_provider, text).await;
    }
    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
    drop(child_provider);
    (delegating.caller, child_id)
}

#[tokio::test]
async fn a_stopping_server_settles_a_brokered_subagents_turn_and_its_row_with_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-spawn-stop";
    let (caller_id, child_id) =
        stop_with_a_brokered_subagent_working(state_dir.path(), channel, None).await;

    let restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert!(child.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. }
            if text == "Provider execution failed: Suru stopped the Provider Session before the Turn completed."
    )));
    let caller = read_session(&descriptor, caller_id).await;
    let (status, duration_ms) = row_status(&caller, child_id);
    assert_eq!(
        status,
        ActivityStatus::Failed,
        "the row settled with the Turn"
    );
    assert!(
        duration_ms.is_some(),
        "timed by the stop that ended the Subagent's work"
    );
    assert_eq!(caller.working_since(), None);

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restart_settles_a_brokered_subagents_open_turn_failed_and_its_row_with_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-spawn-restart";
    let (caller_id, child_id) =
        stop_with_a_brokered_subagent_working(state_dir.path(), channel, None).await;

    reopen_every_turn(state_dir.path(), channel, caller_id);

    let restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Failed);
    assert!(child.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text == "The server stopped before this Subagent finished."
    )));
    assert_eq!(child.working_since(), None);
    let caller = read_session(&descriptor, caller_id).await;
    assert_eq!(
        row_status(&caller, child_id),
        (ActivityStatus::Failed, None),
        "the row settles with the Turn, with no duration since nothing timed its end"
    );
    let Activity::Subagent { brokered, .. } = row_for(&caller, child_id) else {
        unreachable!()
    };
    assert!(
        *brokered,
        "the row read back from storage still says Suru spawned the Subagent through the Broker"
    );
    assert_eq!(caller.working_since(), None);
    assert_eq!(caller.session.status, SessionStatus::Idle);
    assert_eq!(
        child
            .session
            .approval_posture
            .map(|posture| posture.value.provider()),
        Some(ProviderId::new("codex")),
        "the next process still reads the Subagent as brokered, on an actor of its own: it \
         keeps its own posture rather than taking its Claude spawner's"
    );

    restarted.server.shutdown().await.expect("shut down server");
}

/// Puts a stopped Server's history back the way a process that never settled
/// its work would have left it: every Turn open, and the brokered Subagent's
/// row in `caller_id`'s Transcript still live.
fn reopen_every_turn(state_dir: &Path, channel: &str, caller_id: SessionId) {
    use diesel::{Connection, RunQueryDsl, SqliteConnection};
    let config = ServerConfig::new(state_dir, channel).expect("configure server");
    let mut database =
        SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap()).unwrap();
    diesel::sql_query(
        "UPDATE turns SET payload = json_set(payload, '$.status', 'active', '$.settled_at', json('null'))",
    )
    .execute(&mut database)
    .unwrap();
    diesel::sql_query(format!(
        "UPDATE activities SET payload = json_set(payload, '$.status', 'active', '$.duration_ms', json('null')) WHERE session_id = '{caller_id}' AND json_extract(payload, '$.kind') = 'subagent'"
    ))
    .execute(&mut database)
    .unwrap();
}

/// What the Subagents below say last: the answer each settles with.
const FINDING: &str =
    "The Provider seams are ProviderRuntime and ProviderSession, both in src/provider.rs.";

/// Has `provider` write `text` as one whole Agent Message, each event taken
/// up by the Provider actor before the next is sent.
async fn write_agent_message(provider: &ControlledProviderSession, text: &str) {
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: text.to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }
}

/// Has a brokered Subagent's Provider write `text` as one whole Agent
/// Message, and waits until the Subagent's Session holds it.
async fn say(
    descriptor: &RuntimeDescriptor,
    child_id: SessionId,
    provider: &ControlledProviderSession,
    text: &str,
) {
    write_agent_message(provider, text).await;
    read_until(
        descriptor,
        child_id,
        "the Subagent's Message is recorded",
        |snapshot| {
            snapshot.messages.iter().any(|message| {
                message.role == MessageRole::Agent
                    && message.status == MessageStatus::Completed
                    && message.content == text
            })
        },
    )
    .await;
}

#[tokio::test]
async fn reading_a_brokered_subagent_says_it_works_and_for_how_long_then_how_it_settled_and_its_final_message()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-read-subagent", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    let quiet = delegating.client.read_subagent(child_id).await;
    assert_eq!(
        quiet["session_id"],
        json!(child_id),
        "the answer names the Subagent it read"
    );
    assert_eq!(quiet["status"], json!("working"));
    assert_eq!(
        quiet["message"],
        Value::Null,
        "a Subagent that has written nothing yet has no Message to read"
    );
    let first_ms = quiet["duration_ms"].as_u64().unwrap_or_else(|| {
        panic!("a working Subagent says how long it has worked so far: {quiet}")
    });

    say(
        &descriptor,
        child_id,
        &child_provider,
        "Starting with src/provider.rs.",
    )
    .await;
    let working = delegating.client.read_subagent(child_id).await;
    assert_eq!(working["status"], json!("working"));
    assert_eq!(
        working["message"],
        json!("Starting with src/provider.rs."),
        "while it works, the latest Message it has written so far"
    );
    assert!(
        working["duration_ms"]
            .as_u64()
            .is_some_and(|so_far| so_far >= first_ms),
        "its time so far only grows: {first_ms} ms, then {working}"
    );
    let mut fields = working
        .as_object()
        .expect("the answer is a JSON object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["duration_ms", "message", "session_id", "status"],
        "the answer has the shape its description promises"
    );

    say(&descriptor, child_id, &child_provider, FINDING).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the row settles with the child's Turn",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    let (_, row_duration) = row_status(&caller, child_id);
    assert!(row_duration.is_some());
    assert_eq!(
        delegating.client.read_subagent(child_id).await,
        json!({
            "session_id": child_id,
            "status": "completed",
            "duration_ms": row_duration,
            "message": FINDING,
        }),
        "once settled: how it settled, how long it worked as its row says, and its final Message \
         whole"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_whose_turn_failed_or_was_stopped_reads_as_failed_or_stopped() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-read-outcomes", None).await;
    let descriptor = delegating.descriptor.clone();

    let failing = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (failing_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    say(
        &descriptor,
        failing,
        &failing_provider,
        "The sandbox will not let me run the tests.",
    )
    .await;
    failing_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnFailed {
            message: "the sandbox refused the command".to_owned(),
        })
        .await;

    let stopped = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (stopped_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    stopped_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;

    let caller = read_until(
        &descriptor,
        delegating.caller,
        "both rows settle with their children's Turns",
        |snapshot| {
            row_status(snapshot, failing).0 != ActivityStatus::Active
                && row_status(snapshot, stopped).0 != ActivityStatus::Active
        },
    )
    .await;
    for (child, status, message) in [
        (
            failing,
            "failed",
            json!("The sandbox will not let me run the tests."),
        ),
        (stopped, "stopped", Value::Null),
    ] {
        let (_, row_duration) = row_status(&caller, child);
        assert!(row_duration.is_some());
        assert_eq!(
            delegating.client.read_subagent(child).await,
            json!({
                "session_id": child,
                "status": status,
                "duration_ms": row_duration,
                "message": message,
            }),
            "a {status} Subagent reads so, with its last words where it wrote any"
        );
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_a_restart_settled_reads_as_failed_with_no_duration() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-read-restart";
    let (caller_id, child_id) = stop_with_a_brokered_subagent_working(
        state_dir.path(),
        channel,
        Some("Halfway through the seams."),
    )
    .await;
    reopen_every_turn(state_dir.path(), channel, caller_id);

    let mut restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    // The caller's Agent reaches the Broker again once its next Prompt starts
    // its Provider.
    admit_prompt(&descriptor, caller_id, "What did the Researcher find?").await;
    let start = next_start(&mut restarted.claude).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker");
    let mut caller_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    timeout(PROGRESS_DEADLINE, caller_provider.next_turn())
        .await
        .expect("the Prompt reaches the caller's Provider")
        .succeed();
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;

    let caller = read_session(&descriptor, caller_id).await;
    assert_eq!(
        row_status(&caller, child_id),
        (ActivityStatus::Failed, None)
    );
    let child = read_session(&descriptor, child_id).await;
    assert!(
        child.turns[0].settled_at > child.turns[0].started_at,
        "the restart settled the Subagent's Turn where it last showed work, after it began"
    );
    assert_eq!(
        client.read_subagent(child_id).await,
        json!({
            "session_id": child_id,
            "status": "failed",
            "duration_ms": null,
            "message": "Halfway through the seams.",
        }),
        "but nothing timed the end of its work, so it says no duration, as its row does"
    );

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_spawns_a_brokered_subagent_one_level_down_and_the_tree_lists_all_three_levels()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-nested", None).await;
    let descriptor = delegating.descriptor.clone();
    let (tree, mut updates) = open_tree(&descriptor, delegating.caller).await;
    let mut revision = tree.revision;

    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let start = next_start(&mut delegating.hosted.codex).await;
    let child_handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent's start request carries a Broker handoff of its own");
    assert_ne!(child_handoff.token(), delegating.handoff.token());
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();

    // The Subagent's Agent reaches the Broker on its own token, and delegates
    // in its turn.
    let mut child_client = McpClient::handed(&child_handoff);
    child_client.initialize().await;
    let grandchild_id = child_client
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "haiku",
            "name": "Scout",
            "description": "Chase the Codex seam",
            "prompt": DELEGATION,
        }))
        .await;

    let grandchild = read_session(&descriptor, grandchild_id).await;
    assert_eq!(
        grandchild.session.parent,
        Some(child_id),
        "one level down: a child of the Subagent that spawned it"
    );
    let child = read_session(&descriptor, child_id).await;
    let Activity::Subagent { turn_id, .. } = row_for(&child, grandchild_id) else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, child.turns[0].id,
        "its row stands in the Turn the spawning Subagent works in"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert!(
        !caller.activities.iter().any(|activity| matches!(
            activity,
            Activity::Subagent { session_id, .. } if *session_id == grandchild_id
        )),
        "and nowhere above it"
    );
    let spawned = changes_until(&mut updates, &mut revision, |change| {
        matches!(
            change,
            SubagentTreeChange::SubagentSpawned { entry } if entry.session_id == grandchild_id
        )
    })
    .await;
    let Some(SubagentTreeChange::SubagentSpawned { entry }) = spawned.last() else {
        unreachable!()
    };
    assert_eq!(
        entry.parent_session_id, child_id,
        "the Section, watching from the top, learns of it beneath the Subagent that spawned it"
    );

    let start = next_start(&mut delegating.hosted.claude).await;
    let grandchild_handoff = start
        .broker()
        .cloned()
        .expect("the grandchild is handed the Broker too");
    assert!(
        grandchild_handoff.token() != child_handoff.token()
            && grandchild_handoff.token() != delegating.handoff.token(),
        "on a token of its own"
    );
    let mut grandchild_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = timeout(PROGRESS_DEADLINE, grandchild_provider.next_turn())
        .await
        .expect("the Delegation reaches the grandchild's Provider");
    assert_eq!(
        turn.prompt(),
        delegated_by("the Subagent \"Researcher\""),
        "the Delegation names the Subagent that sent it by the name its row carries"
    );
    turn.succeed();

    read_until(
        &descriptor,
        child_id,
        "the grandchild's row carries the Model its Provider confirmed",
        |snapshot| row_model(snapshot, grandchild_id).is_some(),
    )
    .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the child's row carries the Model its Provider confirmed",
        |snapshot| row_model(snapshot, child_id).is_some(),
    )
    .await;
    let (tree, _updates) = open_tree(&descriptor, grandchild_id).await;
    assert_eq!(
        tree.top_level.session_id, delegating.caller,
        "the tree is the same wherever in it the reader stands"
    );
    assert_eq!(
        tree.subagents
            .iter()
            .map(|entry| (
                entry.session_id,
                entry.parent_session_id,
                entry.spawn_order,
                entry.name.as_str(),
                entry.model.clone(),
                entry.status,
            ))
            .collect::<Vec<_>>(),
        [
            (
                child_id,
                delegating.caller,
                0,
                "Researcher",
                Some(ModelId::new("gpt-5.5")),
                ActivityStatus::Active,
            ),
            (
                grandchild_id,
                child_id,
                0,
                "Scout",
                Some(ModelId::new("haiku")),
                ActivityStatus::Active,
            ),
        ],
        "all three levels, each Subagent beneath the one that spawned it in spawn order, with \
         the Model its own Provider confirmed rather than its spawner's"
    );

    // Each Agent above the grandchild reads it: its lineage passes through
    // both.
    for client in [&mut delegating.client, &mut child_client] {
        let read = client.read_subagent(grandchild_id).await;
        assert_eq!(read["session_id"], json!(grandchild_id));
        assert_eq!(read["status"], json!("working"));
    }

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_that_spawns_after_its_turn_settled_still_reads_as_its_own_work_ended()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-read-past-holding", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let start = next_start(&mut delegating.hosted.codex).await;
    let child_handoff = start
        .broker()
        .cloned()
        .expect("the Subagent is handed the Broker");
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    say(&descriptor, child_id, &child_provider, FINDING).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnFailed {
            message: "the sandbox refused the command".to_owned(),
        })
        .await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the child's row settles with its failed Turn",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Failed,
    )
    .await;
    let (_, row_duration) = row_status(&caller, child_id);
    assert!(row_duration.is_some());

    // Its Provider carries on past that Turn and spawns in its turn, so the
    // grandchild's row stands in a Continuation of the child's Session.
    let mut child_client = McpClient::handed(&child_handoff);
    child_client.initialize().await;
    let grandchild_id = child_client
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "haiku",
            "name": "Scout",
            "description": "Chase the Codex seam",
            "prompt": DELEGATION,
        }))
        .await;
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns.len(), 2);
    let Activity::Subagent { turn_id, .. } = row_for(&child, grandchild_id) else {
        unreachable!()
    };
    assert_eq!(
        (*turn_id, child.turns[1].prompt_id, child.turns[1].status),
        (child.turns[1].id, None, TurnStatus::Completed),
        "the Continuation holding the row is the child Session's latest Turn"
    );

    assert_eq!(
        delegating.client.read_subagent(child_id).await,
        json!({
            "session_id": child_id,
            "status": "failed",
            "duration_ms": row_duration,
            "message": FINDING,
        }),
        "the child still reads as its own work ended: a Continuation that only holds a row is no \
         stretch of its work"
    );
    let (tree, _updates) = open_tree(&descriptor, delegating.caller).await;
    let entry = tree
        .subagents
        .iter()
        .find(|entry| entry.session_id == child_id)
        .expect("the tree lists the child");
    assert_eq!(
        (entry.status, entry.worked_ms),
        (ActivityStatus::Failed, row_duration),
        "and its entry in the Subagents Section keeps its outcome and its time"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn reading_an_id_that_is_not_a_brokered_subagent_beneath_the_caller_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-read-refused", None).await;
    let descriptor = delegating.descriptor.clone();
    let workspace = delegating.hosted.workspace.path().to_owned();

    // The caller's own Provider spawns a native Subagent.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the seams".to_owned(),
            delegation: None,
        })
        .await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the native Subagent's row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Some(Activity::Subagent {
        session_id: native_id,
        ..
    }) = caller
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        unreachable!()
    };
    let native_id = *native_id;

    // Two brokered Subagents side by side, the first taken up by its Provider.
    let first = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let start = next_start(&mut delegating.hosted.codex).await;
    let first_handoff = start
        .broker()
        .cloned()
        .expect("the Subagent is handed the Broker");
    let mut first_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    timeout(PROGRESS_DEADLINE, first_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    let second = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;

    // And another tree altogether, with a brokered Subagent of its own.
    let (_stranger, stranger_handoff, _stranger_provider) = start_session(
        &descriptor,
        &mut delegating.hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;
    let mut stranger_client = McpClient::handed(&stranger_handoff);
    stranger_client.initialize().await;
    let strangers = stranger_client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;

    let beneath = "is not a Subagent spawned with spawn_subagent by you or by a Subagent beneath \
                   you";
    for (arguments, says) in [
        (json!({ "id": strangers }), beneath),
        (json!({ "id": native_id }), beneath),
        (json!({ "id": delegating.caller }), beneath),
        (json!({ "id": SessionId::new() }), "Suru holds no Session"),
        (
            json!({ "id": "the Researcher" }),
            "`id` must be the session_id spawn_subagent answered with",
        ),
        (json!({}), "read_subagent needs `id`"),
        (
            json!({ "id": first, "tail": 10 }),
            "read_subagent takes no argument `tail`",
        ),
    ] {
        let refusal = delegating
            .client
            .refusal("read_subagent", arguments.clone())
            .await;
        assert!(
            refusal.contains(says),
            "reading {arguments} is refused saying {says:?}, but said {refusal:?}"
        );
    }

    // A brokered Subagent reads only what lies beneath it: neither the
    // Session that spawned it nor a sibling.
    let mut first_client = McpClient::handed(&first_handoff);
    first_client.initialize().await;
    for id in [delegating.caller, second] {
        let refusal = first_client
            .refusal("read_subagent", json!({ "id": id }))
            .await;
        assert!(refusal.contains(beneath), "{refusal}");
    }
    assert_eq!(
        delegating.client.read_subagent(first).await["status"],
        json!("working"),
        "while the caller reads its own"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
