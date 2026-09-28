//! The MCP client a Provider harness is to the Broker: JSON-RPC posted to the
//! streamable-HTTP endpoint on the Server's loopback listener, under the bearer
//! token a Session's Provider start request carried (ADR 0034). The Provider
//! suites act as this client wherever an Agent would call a Broker Tool.

use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::{Value, json};
use suru::{protocol::SessionId, provider::BrokerHandoff};

/// The MCP revision the client speaks, as the harnesses Suru hosts do today.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP client a Provider harness is: JSON-RPC posted to the Broker
/// endpoint under the bearer token its start request carried.
pub struct McpClient {
    http: reqwest::Client,
    endpoint: String,
    authorization: Option<String>,
    /// What every Tool call carries as its `_meta`, if anything.
    call_meta: Option<Value>,
    next_id: u64,
}

impl McpClient {
    pub fn handed(handoff: &BrokerHandoff) -> Self {
        Self::presenting(handoff.endpoint().as_str(), Some(handoff.token().bearer()))
    }

    pub fn presenting(endpoint: &str, authorization: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.to_owned(),
            authorization,
            call_meta: None,
            next_id: 0,
        }
    }

    /// This client, carrying `meta` as the `_meta` of every Tool call it
    /// makes from here on — as each Codex thread names itself in every call
    /// it makes, `{"threadId": ...}`, a native Subagent's own thread included,
    /// under the token its parent's start carried.
    pub fn with_call_meta(mut self, meta: Value) -> Self {
        self.call_meta = Some(meta);
        self
    }

    async fn post(&self, message: &Value) -> reqwest::Response {
        let mut request = self
            .http
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
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
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
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
    pub async fn initialize_status(&mut self) -> StatusCode {
        let (_, message) = self.request_message("initialize", initialize_params());
        self.post(&message).await.status()
    }

    /// The handshake every MCP session opens with: `initialize`, then the
    /// notification that the client is ready.
    pub async fn initialize(&mut self) -> Value {
        let result = self.request("initialize", initialize_params()).await;
        let initialized = self
            .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert_eq!(initialized.status(), StatusCode::ACCEPTED);
        result
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let mut params = json!({ "name": name, "arguments": arguments });
        if let Some(meta) = &self.call_meta {
            params["_meta"] = meta.clone();
        }
        self.request("tools/call", params).await
    }

    /// `list_providers`' answer, read from the structured content the call
    /// carries, having checked the text content says the same.
    pub async fn list_providers(&mut self) -> Value {
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

    /// Spawns a brokered Subagent and answers with its Session, read from the
    /// structured content the call carries.
    pub async fn spawn_subagent(&mut self, arguments: Value) -> SessionId {
        let result = self.call_tool("spawn_subagent", arguments).await;
        assert_ne!(
            result["isError"],
            json!(true),
            "spawn_subagent answers: {result}"
        );
        serde_json::from_value(result["structuredContent"]["session_id"].clone())
            .unwrap_or_else(|_| panic!("the answer names the Subagent's Session: {result}"))
    }

    /// What `tool` refuses a call with: the Tool's own error, in words the
    /// calling Agent reads.
    pub async fn refusal(&mut self, tool: &str, arguments: Value) -> String {
        let result = self.call_tool(tool, arguments).await;
        assert_eq!(result["isError"], json!(true), "{tool} refuses: {result}");
        result["content"][0]["text"]
            .as_str()
            .expect("a refusal is told in text")
            .to_owned()
    }

    /// `read_subagent`'s answer for `id`, read from the structured content
    /// the call carries.
    pub async fn read_subagent(&mut self, id: SessionId) -> Value {
        let result = self.call_tool("read_subagent", json!({ "id": id })).await;
        assert_ne!(
            result["isError"],
            json!(true),
            "read_subagent answers: {result}"
        );
        result["structuredContent"].clone()
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
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

/// What a Tool call answered, with every progress notification the Broker sent
/// against the call's `progressToken` before its answer, in the order they
/// came.
pub struct Observed {
    pub result: Value,
    pub progress: Vec<Value>,
}

impl Observed {
    /// The answer's structured content, having checked the call was not
    /// refused.
    pub fn answer(&self, tool: &str) -> Value {
        assert_ne!(
            self.result["isError"],
            json!(true),
            "{tool} answers: {}",
            self.result
        );
        self.result["structuredContent"].clone()
    }
}

impl McpClient {
    /// `send_to_subagent`'s answer for sending `message` to `id`, read from
    /// the structured content the call carries.
    pub async fn send_to_subagent(&mut self, id: SessionId, message: &str) -> Value {
        self.observe_tool(
            "send_to_subagent",
            json!({ "id": id, "message": message }),
            None,
            None,
        )
        .await
        .answer("send_to_subagent")
    }

    /// `wait_subagents`' answer for `arguments`, asked for without progress.
    pub async fn wait_subagents(&mut self, arguments: Value) -> Value {
        self.observe_tool("wait_subagents", arguments, None, None)
            .await
            .answer("wait_subagents")
    }

    /// Calls `wait_subagents` asking for progress against `progress_token`,
    /// reading its answer as it streams: each progress notification is sent
    /// on `progress_seen` the moment it arrives — so a test may act while the
    /// wait still waits — and all of them are handed back beside the answer.
    pub async fn wait_subagents_observing(
        &mut self,
        arguments: Value,
        progress_token: &str,
        progress_seen: &tokio::sync::mpsc::UnboundedSender<Value>,
    ) -> Observed {
        self.observe_tool(
            "wait_subagents",
            arguments,
            Some(progress_token),
            Some(progress_seen),
        )
        .await
    }

    /// Calls `name`, asking for progress against `progress_token` where one
    /// is given, and reads the answer as it streams. A plain JSON answer
    /// carries no progress.
    async fn observe_tool(
        &mut self,
        name: &str,
        arguments: Value,
        progress_token: Option<&str>,
        progress_seen: Option<&tokio::sync::mpsc::UnboundedSender<Value>>,
    ) -> Observed {
        let mut params = json!({ "name": name, "arguments": arguments });
        if let Some(token) = progress_token {
            params["_meta"] = json!({ "progressToken": token });
        }
        let (id, message) = self.request_message("tools/call", params);
        let mut response = self.post(&message).await;
        assert_eq!(response.status(), StatusCode::OK, "{name} is answered");
        let is_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_stream {
            let answer: Value = response.json().await.expect("decode JSON-RPC response");
            return Observed {
                result: answered(name, answer),
                progress: Vec::new(),
            };
        }
        let mut progress = Vec::new();
        let mut buffered = String::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .expect("read the event stream")
                .unwrap_or_else(|| panic!("the event stream ends with the answer to {name}"));
            buffered.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
            while let Some(end) = buffered.find("\n\n") {
                let event = buffered[..end].to_owned();
                buffered.drain(..end + 2);
                let data = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                let Ok(message) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if message["method"] == json!("notifications/progress") {
                    if let Some(seen) = progress_seen {
                        let _ = seen.send(message["params"].clone());
                    }
                    progress.push(message["params"].clone());
                } else if message["id"] == json!(id) {
                    return Observed {
                        result: answered(name, message),
                        progress,
                    };
                }
            }
        }
    }
}

/// The result a JSON-RPC response to a call of `tool` carries, failing the
/// test on a protocol error.
fn answered(tool: &str, answer: Value) -> Value {
    answer
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{tool} was answered with an error: {answer}"))
}
