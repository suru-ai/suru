//! The Broker as MCP: rmcp's streamable-HTTP server transport mounted on the
//! Server's loopback router, behind a gate that answers nothing while the
//! Broker is off and refuses any request whose bearer token names no live
//! Session.
//!
//! What a request is answered also depends on who makes it: `tools/list` names
//! only the Tools its caller is offered, and a call of a Tool its caller is not
//! offered is answered as a call of a Tool the Broker does not have, so a
//! Sidekick's Tools are neither shown to nor run for anyone else (ADR 0042).
//!
//! The transport runs statelessly: every POST is answered on its own, with no
//! MCP session for the Server to keep or expire when a Provider goes away, and
//! a request's caller is whatever its own token names — or, where a call's
//! `_meta` names a native Subagent riding that token's Provider connection,
//! that Subagent (see [`calling_agent`]). An answer is plain JSON
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
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ProgressNotificationParam, RequestMetaObject, ServerCapabilities, Tool, ToolAnnotations,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde_json::Value;
use tokio::sync::watch;

use super::{
    BROKER_PATH, BrokerAccess, BrokerCaller,
    tools::{BrokerTool, BrokerTools, ProgressReporter, ToolCall, ToolProgress},
};
use crate::provider::{ProviderSubagentId, wait_for_shutdown};

/// What the Broker tells an Agent about itself as the MCP session opens.
const INSTRUCTIONS: &str = "\
Suru's Broker offers Tools for working across every Provider Suru hosts, \
beside the Tools your own Provider gives you. Call list_providers to learn \
which Providers, Models and Model Options may be chosen, and spawn_subagent to \
delegate a piece of work to a Subagent on any of them. \
Call read_subagent with the session_id spawn_subagent answered with to learn \
how that Subagent is doing and read what it last wrote, send_to_subagent to \
send it more work, and stop_subagent to stop one whose work you no longer need. \
When a Subagent settles, Suru reports it to you as a new message that wakes you \
if your turn has ended, so once you have nothing left to do but wait on one, \
end your turn. Call wait_subagents only when you must have a Subagent's result \
before your turn ends.";

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

/// The Provider's own identity for the Agent making a call, where the call's
/// `_meta` names one. Codex names the thread making each call in
/// `_meta.threadId` — a native Subagent's own thread for that Subagent's call
/// — and a native Codex Subagent is known by its thread's id. The
/// `_meta.sessionId` beside it names the thread at the top of the tree
/// whichever thread calls, so it is never read. Claude's and Copilot's calls
/// name no Agent Suru knows a Session by, so they name none here
/// (`docs/validation/0408-subagent-mcp-attribution.md`).
fn calling_agent(meta: &RequestMetaObject) -> Option<ProviderSubagentId> {
    meta.get("threadId")
        .and_then(Value::as_str)
        .map(ProviderSubagentId::new)
}

impl ServerHandler for BrokerServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("suru", env!("CARGO_PKG_VERSION")).with_title("Suru Broker"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    /// Every Tool the Broker serves, saying — as the 2026-07-28 revision
    /// requires of every list — how long the list stays fresh and who may
    /// cache it. Claude 2.1.283 speaks that revision to the Broker once
    /// `server/discover` offers it, and registers none of the Tools from a list
    /// that says neither (`docs/validation/0421-broker-smoke.md`); earlier
    /// revisions carry no such fields, and their clients pass over them.
    ///
    /// The answer promises nothing a harness could hold on to: it is stale at
    /// once, so a harness asks again whenever it wants the list, and it is for
    /// the holder of the token that asked alone — which matters, since what it
    /// lists is what that token's caller is offered. rmcp answers
    /// `server/discover` the same way.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let Some(caller) = caller(&context) else {
            return Err(unidentified_caller());
        };
        Ok(ListToolsResult::with_all_items(
            BrokerTool::offered_to(caller.role())
                .map(described)
                .collect(),
        )
        .with_ttl_ms(0)
        .with_cache_scope(CacheScope::Private))
    }

    /// A Tool's description, which rmcp reads for the shape of its arguments
    /// alone; whether a caller is offered the Tool is asked where it is listed
    /// and where it is called.
    fn get_tool(&self, name: &str) -> Option<Tool> {
        BrokerTool::named(name).map(described)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(caller) = caller(&context) else {
            return Err(unidentified_caller());
        };
        let caller = self
            .tools
            .attribute(caller, calling_agent(&context.meta).as_ref());
        // A Tool the caller is not offered is no Tool of the Broker's as far
        // as that caller can tell, so the refusal says no more than it would
        // of a name the Broker never had.
        let Some(tool) =
            BrokerTool::named(&request.name).filter(|tool| tool.is_offered_to(caller.role()))
        else {
            return Err(ErrorData::invalid_params(
                format!("The Broker offers no Tool named `{}`", request.name),
                None,
            ));
        };
        let call = ToolCall {
            caller,
            arguments: request.arguments.unwrap_or_default(),
            progress: progress_reporter(&context),
        };
        // A call whose client has gone is cancelled, and a wait stops waiting
        // with it: while the call has answered nothing, once the transport
        // sees the connection close; once it streams progress, when the
        // stream's next write — progress, or the transport's keep-alive every
        // 15 seconds — finds the connection closed and the stream is dropped;
        // and when a stopping Server ends every stream. A client's
        // `notifications/cancelled` comes on a request of its own, which this
        // stateless transport ties to no call, so it cancels nothing; the
        // wait's own timeout bounds it then.
        let answered = tokio::select! {
            answered = self.tools.call(tool, call) => answered,
            () = context.ct.cancelled() => {
                return Err(ErrorData::internal_error("The Broker call was cancelled", None));
            }
        };
        let result = match answered {
            Ok(answer) => CallToolResult::structured(answer),
            Err(refusal) => CallToolResult::error(vec![ContentBlock::text(refusal.to_string())]),
        };
        Ok(result.into())
    }
}

/// What a request is refused with when the gate in [`serve`] carried no
/// caller to it, which every request the transport is handed carries.
fn unidentified_caller() -> ErrorData {
    ErrorData::internal_error("The Broker could not tell which Session is calling", None)
}

/// Where a call reports its progress: an MCP progress notification against
/// the `progressToken` it carried, sent ahead of its answer — which turns that
/// answer into an event stream at once — or nowhere for a call that carried
/// none, since a notification may only name a token its call gave.
fn progress_reporter(context: &RequestContext<RoleServer>) -> Option<ProgressReporter> {
    let token = context.meta.get_progress_token()?;
    let peer = context.peer.clone();
    Some(Box::new(move |progress: ToolProgress| {
        let peer = peer.clone();
        let notification = ProgressNotificationParam::new(token.clone(), progress.progress)
            .with_total(progress.total)
            .with_message(progress.message);
        Box::pin(async move {
            // A client gone before its answer hears nothing more; the call
            // stops with its cancellation.
            if let Err(error) = peer.notify_progress(notification).await {
                tracing::debug!("a Broker call's progress went unheard: {error}");
            }
        })
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::{
        Method,
        header::{ACCEPT, CONTENT_TYPE, HOST},
    };
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

    /// A Tool that reports progress until its call is cancelled, and says when
    /// it was, as a wait still waiting does.
    struct ReportingUntilCancelled {
        cancelled: tokio::sync::mpsc::UnboundedSender<()>,
    }

    impl ServerHandler for ReportingUntilCancelled {
        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            let token = context
                .meta
                .get_progress_token()
                .expect("the call asked for progress");
            let mut progress = 0.0;
            loop {
                tokio::select! {
                    () = context.ct.cancelled() => {
                        let _ = self.cancelled.send(());
                        return Err(ErrorData::internal_error("cancelled", None));
                    }
                    () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                        progress += 1.0;
                        let _ = context
                            .peer
                            .notify_progress(ProgressNotificationParam::new(token.clone(), progress))
                            .await;
                    }
                }
            }
        }
    }

    /// A call whose answer is already streaming progress is still cancelled
    /// once its client goes: the event stream, dropped when a write finds the
    /// connection closed, cancels the call — which is how a wait stops waiting
    /// for a harness that gave up on it, at the next progress or keep-alive
    /// it writes.
    #[tokio::test]
    async fn a_call_streaming_progress_is_cancelled_once_its_client_goes() {
        let (cancelled, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let transport = StreamableHttpService::new(
            move || {
                Ok(ReportingUntilCancelled {
                    cancelled: cancelled.clone(),
                })
            },
            Arc::new(NeverSessionManager::default()),
            transport_config(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("the listener has an address");
        let app = Router::new().route(
            BROKER_PATH,
            any(move |request: Request| {
                let transport = transport.clone();
                async move { transport.handle(request).await.map(Body::new) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let mut response = reqwest::Client::new()
            .post(format!("http://{address}{BROKER_PATH}"))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "wait",
                        "arguments": {},
                        "_meta": { "progressToken": "wait-1" },
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .expect("reach the transport");
        let mut streamed = String::new();
        while !streamed.contains("notifications/progress") {
            let chunk = response
                .chunk()
                .await
                .expect("read the event stream")
                .expect("the call streams progress before it answers");
            streamed.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            heard.try_recv().is_err(),
            "a call its client still reads is not cancelled"
        );

        drop(response);
        tokio::time::timeout(std::time::Duration::from_secs(10), heard.recv())
            .await
            .expect("the call is cancelled once its client has gone")
            .expect("the Tool says it was cancelled");
        server.abort();
    }
}
