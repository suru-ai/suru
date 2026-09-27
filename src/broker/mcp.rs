//! The Broker as MCP: rmcp's streamable-HTTP server transport mounted on the
//! Server's loopback router, behind a gate that answers nothing while the
//! Broker is off and refuses any request whose bearer token names no live
//! Session.
//!
//! The transport runs statelessly: every POST is answered on its own, with no
//! MCP session for the Server to keep or expire when a Provider goes away, and
//! a request's caller is whatever its own token names. An answer is plain JSON
//! unless the Tool reports progress before it finishes, when rmcp answers with
//! an event stream instead so nothing is lost — which is how a long call keeps
//! a harness's idle window open: take the progress token from
//! `context.meta.get_progress_token()` and send `context.peer.notify_progress`.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header::WWW_AUTHENTICATE, request::Parts},
    response::{IntoResponse, Response},
    routing::any,
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
        ToolAnnotations,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio::sync::watch;

use super::{
    BROKER_PATH, BrokerAccess, BrokerCaller,
    tools::{BrokerTool, BrokerTools, ToolCall},
};
use crate::provider::wait_for_shutdown;

/// What the Broker tells an Agent about itself as the MCP session opens.
const INSTRUCTIONS: &str = "\
Suru's Broker offers Tools for working across every Provider Suru hosts, \
beside the Tools your own Provider gives you. Call list_providers to learn \
which Providers, Models and Model Options may be chosen, and spawn_subagent to \
delegate a piece of work to a Subagent on any of them.";

type Transport = StreamableHttpService<BrokerServer, NeverSessionManager>;

#[derive(Clone)]
struct BrokerRoute {
    access: BrokerAccess,
    transport: Transport,
}

/// The Broker's one route, for the loopback router to merge. The transport
/// stops answering its streams once `shutdown` is signalled, so a Server
/// shutting down never waits on a long Tool call.
pub(super) fn router(
    access: BrokerAccess,
    tools: BrokerTools,
    mut shutdown: watch::Receiver<bool>,
) -> Router {
    let config = transport_config();
    let stop = config.cancellation_token.clone();
    tokio::spawn(async move {
        wait_for_shutdown(&mut shutdown).await;
        stop.cancel();
    });
    let transport = StreamableHttpService::new(
        move || Ok(BrokerServer::new(tools.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route(BROKER_PATH, any(serve))
        .with_state(BrokerRoute { access, transport })
}

/// Stateless, answering in JSON wherever a Tool reports nothing before its
/// result, and — rmcp's default — accepting only loopback `Host` headers.
fn transport_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
}

/// Answers nothing while the Broker is off, refuses a request presenting no
/// live token, and otherwise hands the request to the transport carrying the
/// Session its token names.
async fn serve(State(route): State<BrokerRoute>, mut request: Request) -> Response {
    if !route.access.is_enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(caller) = route.access.caller(request.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            "The Broker token is unknown or has been retired",
        )
            .into_response();
    };
    request.extensions_mut().insert(caller);
    route.transport.handle(request).await.map(Body::new)
}

/// One request's view of the Broker as an MCP server. rmcp builds one per
/// request, so it holds nothing a request could leave behind for the next.
struct BrokerServer {
    tools: BrokerTools,
}

impl BrokerServer {
    fn new(tools: BrokerTools) -> Self {
        Self { tools }
    }
}

fn described(tool: BrokerTool) -> Tool {
    Tool::new(tool.name(), tool.description(), tool.input_schema())
        .with_title(tool.title())
        .with_annotations(
            ToolAnnotations::with_title(tool.title())
                .read_only(tool.is_read_only())
                .open_world(false),
        )
}

/// The Session whose token the request carried, as the gate in [`serve`]
/// resolved it before the transport saw the request.
fn caller(context: &RequestContext<RoleServer>) -> Option<BrokerCaller> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<BrokerCaller>())
        .copied()
}

impl ServerHandler for BrokerServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("suru", env!("CARGO_PKG_VERSION")).with_title("Suru Broker"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            BrokerTool::ALL.into_iter().map(described).collect(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        BrokerTool::named(name).map(described)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = BrokerTool::named(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("The Broker offers no Tool named `{}`", request.name),
                None,
            ));
        };
        let Some(caller) = caller(&context) else {
            return Err(ErrorData::internal_error(
                "The Broker could not tell which Session is calling",
                None,
            ));
        };
        let call = ToolCall {
            caller,
            arguments: request.arguments.unwrap_or_default(),
        };
        let result = match self.tools.call(tool, call).await {
            Ok(answer) => CallToolResult::structured(answer),
            Err(refusal) => CallToolResult::error(vec![ContentBlock::text(refusal.to_string())]),
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{
        Method,
        header::{ACCEPT, CONTENT_TYPE, HOST},
    };
    use rmcp::model::ProgressNotificationParam;
    use serde_json::{Value, json};

    use super::*;

    /// A Tool that reports progress before it answers, as a long wait must so
    /// a harness's idle window never closes on it.
    struct ReportingProgress;

    impl ServerHandler for ReportingProgress {
        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            let token = context
                .meta
                .get_progress_token()
                .expect("the call asked for progress");
            context
                .peer
                .notify_progress(ProgressNotificationParam::new(token, 1.0).with_message("waiting"))
                .await
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            Ok(CallToolResult::structured(json!({ "settled": true })).into())
        }
    }

    /// The transport the Broker is served over carries progress notifications
    /// ahead of a long call's answer: its JSON answers turn into an event
    /// stream as soon as a Tool reports progress, rather than losing it.
    #[tokio::test]
    async fn a_tool_reporting_progress_is_answered_with_its_progress_first() {
        let transport = StreamableHttpService::new(
            || Ok(ReportingProgress),
            Arc::new(NeverSessionManager::default()),
            transport_config(),
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri(BROKER_PATH)
            .header(HOST, "127.0.0.1")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {
                        "name": "wait",
                        "arguments": {},
                        "_meta": { "progressToken": "wait-7" },
                    },
                })
                .to_string(),
            ))
            .expect("a well-formed request");

        let response = transport.handle(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream")),
            "progress turns the answer into an event stream"
        );
        let body = axum::body::to_bytes(Body::new(response.into_body()), 64 * 1024)
            .await
            .expect("read the event stream");
        let messages = String::from_utf8(body.to_vec())
            .expect("the event stream is text")
            .split("\n\n")
            .filter_map(|event| {
                let data = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<String>();
                serde_json::from_str::<Value>(&data).ok()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            messages
                .iter()
                .map(|message| {
                    message["method"]
                        .as_str()
                        .map_or_else(|| format!("result {}", message["id"]), str::to_owned)
                })
                .collect::<Vec<_>>(),
            ["notifications/progress", "result 7"]
        );
        assert_eq!(messages[0]["params"]["progressToken"], json!("wait-7"));
        assert_eq!(
            messages[1]["result"]["structuredContent"],
            json!({ "settled": true })
        );
    }
}
