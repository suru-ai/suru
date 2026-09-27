//! The Broker: Suru's own Tools, served as a streamable-HTTP MCP endpoint on
//! the Server's loopback listener and reached with the bearer token a Session's
//! Provider start request carried (ADR 0034).
//!
//! Each test acts as the MCP client a Provider harness is — posting JSON-RPC to
//! the endpoint the controlled double was handed — and asserts only on what
//! that client and the doubles observe. The doubles are hosted under the real
//! Provider identities, because Enablement is a Setting keyed by them.

use std::{path::Path, sync::Arc};

use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::{Value, json};
use suru::{
    protocol::{
        AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, CreateSessionRequest,
        InitialPrompt, ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionRole, PromptDelivery, PromptId, ProviderId, ProviderUnavailability,
        RuntimeDescriptor, SessionId, SettingMutation, TurnStatus,
    },
    provider::{BrokerHandoff, ProviderErrand, ProviderEvent},
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

use crate::{
    provider_support::{
        ControlledProvider, ControlledProviderRuntime, ControlledProviderSession, StartRequest,
    },
    server_support::{PROGRESS_DEADLINE, read_runtime_descriptor},
    support::{create_session, read_session_until},
};

/// The MCP revision the test client speaks, as the harnesses Suru hosts do
/// today.
const PROTOCOL_VERSION: &str = "2025-06-18";

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

/// The MCP client a Provider harness is: JSON-RPC posted to the Broker
/// endpoint under the bearer token its start request carried.
struct McpClient {
    http: reqwest::Client,
    endpoint: String,
    authorization: Option<String>,
    next_id: u64,
}

impl McpClient {
    fn handed(handoff: &BrokerHandoff) -> Self {
        Self::presenting(handoff.endpoint().as_str(), Some(handoff.token().bearer()))
    }

    fn presenting(endpoint: &str, authorization: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.to_owned(),
            authorization,
            next_id: 0,
        }
    }

    async fn post(&self, message: &Value) -> reqwest::Response {
        let mut request = self
            .http
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .json(message);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        request.send().await.expect("reach the Broker endpoint")
    }

    fn request_message(&mut self, method: &str, params: Value) -> (u64, Value) {
        self.next_id += 1;
        let id = self.next_id;
        (
            id,
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )
    }

    /// Sends one request and answers with its JSON-RPC result, failing the
    /// test on anything else.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let (id, message) = self.request_message(method, params);
        let response = self.post(&message).await;
        assert_eq!(response.status(), StatusCode::OK, "{method} is answered");
        let answer = json_rpc_response(response, id).await;
        answer
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} was answered with an error: {answer}"))
    }

    /// What the endpoint answers an `initialize` with, before anything is read
    /// from its body: the one observable a refused caller gets.
    async fn initialize_status(&mut self) -> StatusCode {
        let (_, message) = self.request_message("initialize", initialize_params());
        self.post(&message).await.status()
    }

    /// The handshake every MCP session opens with: `initialize`, then the
    /// notification that the client is ready.
    async fn initialize(&mut self) -> Value {
        let result = self.request("initialize", initialize_params()).await;
        let initialized = self
            .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert_eq!(initialized.status(), StatusCode::ACCEPTED);
        result
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// `list_providers`' answer, read from the structured content the call
    /// carries, having checked the text content says the same.
    async fn list_providers(&mut self) -> Value {
        let result = self.call_tool("list_providers", json!({})).await;
        assert_ne!(
            result["isError"],
            json!(true),
            "list_providers answers: {result}"
        );
        let structured = result["structuredContent"].clone();
        let text = result["content"][0]["text"]
            .as_str()
            .expect("the answer is also given as text");
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("the text is the same JSON"),
            structured,
            "a client reading only text content reads the same answer"
        );
        structured
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "suru-broker-test", "version": "0" },
    })
}

/// The JSON-RPC response `id` names, whether the endpoint answered with JSON
/// or — as it may for a call that reports progress first — an event stream.
async fn json_rpc_response(response: reqwest::Response, id: u64) -> Value {
    let is_stream = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_stream {
        return response.json().await.expect("decode JSON-RPC response");
    }
    let body = response.text().await.expect("read the event stream");
    body.split("\n\n")
        .filter_map(|event| {
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str::<Value>(&data).ok()
        })
        .find(|message| message["id"] == json!(id))
        .unwrap_or_else(|| panic!("the event stream carries the response to {id}: {body}"))
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
    assert_eq!(initialized["protocolVersion"], json!(PROTOCOL_VERSION));
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
        ["list_providers"],
        "list_providers is the one Tool the Broker offers so far"
    );
    assert_eq!(tools[0]["inputSchema"]["type"], json!("object"));
    assert!(
        tools[0]["description"]
            .as_str()
            .is_some_and(|description| description.contains("\"providers\"")),
        "the Tool's description documents the shape it answers with: {}",
        tools[0]
    );
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], json!(true));

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
