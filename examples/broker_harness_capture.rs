//! Records how each installed harness treats an MCP server served the way the Broker will be.
//!
//! Serves a minimal streamable-HTTP MCP endpoint on loopback and drives one installed CLI
//! against it, the way Suru's Provider for that CLI does. Every HTTP exchange the endpoint sees,
//! and every message the driver exchanges with the CLI, is written to stdout as one JSON line
//! (`{"t": seconds since start, "source", "event", "body"}`), in order.
//!
//! The endpoint offers two Tools:
//!
//! - `echo_context` records the call (headers, `params`, `_meta`) and returns a short text.
//! - `slow_tool` runs for `seconds`, answering over `text/event-stream` when the client accepts
//!   it and sending `notifications/progress` every `--progress` seconds when the call carried a
//!   `progressToken`.
//!
//! Nothing runs unless `SURU_BROKER_CAPTURE=1` is set, so the program is inert in any suite.
//!
//! ```sh
//! export SURU_BROKER_CAPTURE=1
//! cargo run --example broker_harness_capture -- serve [--progress 30] [--response sse|json]
//! cargo run --example broker_harness_capture -- codex thread-config      # start, resume, resume bare
//! cargo run --example broker_harness_capture -- codex launch-overrides   # `-c` at app-server launch
//! cargo run --example broker_harness_capture -- codex bearer-token       # inline bearer_token
//! cargo run --example broker_harness_capture -- codex no-approve         # no default approval mode
//! cargo run --example broker_harness_capture -- codex subagent           # a native child's call
//! cargo run --example broker_harness_capture -- claude attribution [--background 1]  # and a subagent
//! cargo run --example broker_harness_capture -- claude approval [--allow 1]  # allowlist off or on
//! cargo run --example broker_harness_capture -- claude slow --seconds 420 [--progress 30]
//!     [--response sse|json] [--timeout-ms 120000] [--env MCP_TOOL_TIMEOUT=120000]
//! cargo run --example broker_harness_capture -- copilot attribution
//! cargo run --example broker_harness_capture -- copilot two-sessions     # two tokens, one process
//! cargo run --example broker_harness_capture -- copilot resume           # token re-minted on resume
//! cargo run --example broker_harness_capture -- copilot slow --seconds 90 [--progress 30]
//!     [--timeout-ms 300000]
//! ```
//!
//! Every Codex scenario takes `--reviewer user|auto_review`, sent as the thread's
//! `approvalsReviewer`; without it the user's own Codex configuration decides.
//!
//! `SURU_CODEX_PATH`, `SURU_CLAUDE_PATH` and `SURU_COPILOT_PATH` name the CLIs (default `codex`,
//! `claude`, `copilot`); `--model` overrides each driver's default Model (`gpt-5.6-luna` at low
//! effort, `haiku`, `gpt-5-mini`). The Claude driver removes every inherited `CLAUDE*` variable
//! so a capture run from inside a Claude Code session starts a clean CLI. Scrub account details
//! from the output before committing any of it. What the captures showed is recorded in
//! `docs/validation/0408-*.md`.
use std::{
    collections::BTreeMap,
    convert::Infallible,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::Response,
    routing::post,
};
use github_copilot_sdk::{
    CliProgram, Client, ClientOptions, IndexMap, PermissionRequestData, RequestId, SessionId,
    handler::{PermissionHandler, PermissionResult},
    session::Session,
    types::{
        McpHttpServerConfig, McpServerConfig, MessageOptions, ResumeSessionConfig, SessionConfig,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};

const GATE: &str = "SURU_BROKER_CAPTURE";
const SERVER: &str = "capture";

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn log(source: &str, event: &str, body: Value) {
    let t = (START.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    println!(
        "{}",
        json!({"t": t, "source": source, "event": event, "body": body})
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var(GATE).as_deref() != Ok("1") {
        eprintln!("broker_harness_capture drives live CLIs; set {GATE}=1 to run it.");
        return Ok(());
    }
    LazyLock::force(&START);
    let mut args = std::env::args().skip(1).peekable();
    let mode = args.next().unwrap_or_default();
    let scenario = args
        .next_if(|argument| !argument.starts_with("--"))
        .unwrap_or_default();
    let options = Options::parse(args)?;
    log(
        "driver",
        "capture.start",
        json!({
            "mode": mode,
            "scenario": scenario,
            "options": options.raw,
            "startedAt": time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?,
        }),
    );
    match mode.as_str() {
        "serve" => {
            let url = start_server(options.server()?).await?;
            eprintln!("serving {url}; Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        "codex" => codex(&scenario, &options).await,
        "claude" => claude(&scenario, &options).await,
        "copilot" => copilot(&scenario, &options).await,
        other => bail!("unknown mode `{other}`; expected serve, codex, claude, or copilot"),
    }
}

/// `--key value` pairs after the mode and scenario. `--env` may repeat.
struct Options {
    raw: BTreeMap<String, String>,
    env: Vec<(String, String)>,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut raw = BTreeMap::new();
        let mut env = Vec::new();
        while let Some(key) = args.next() {
            let key = key
                .strip_prefix("--")
                .ok_or_else(|| anyhow!("expected --key, found `{key}`"))?
                .to_owned();
            let value = args
                .next()
                .ok_or_else(|| anyhow!("--{key} needs a value"))?;
            if key == "env" {
                let (name, value) = value
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--env needs NAME=VALUE"))?;
                env.push((name.to_owned(), value.to_owned()));
            }
            raw.insert(key, value);
        }
        Ok(Self { raw, env })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.raw.get(key).map(String::as_str)
    }

    fn number(&self, key: &str) -> anyhow::Result<Option<u64>> {
        self.get(key)
            .map(|value| value.parse().with_context(|| format!("--{key} `{value}`")))
            .transpose()
    }

    fn server(&self) -> anyhow::Result<ServerOptions> {
        let progress = self.number("progress")?.unwrap_or(30);
        Ok(ServerOptions {
            progress_every: (progress > 0).then(|| Duration::from_secs(progress)),
            json_only: self.get("response") == Some("json"),
        })
    }

    fn model<'a>(&'a self, default: &'a str) -> &'a str {
        self.get("model").unwrap_or(default)
    }
}

// ------------------------------------------------------------------------------------------------
// The MCP endpoint
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ServerOptions {
    /// `None` keeps a slow call silent until its result.
    progress_every: Option<Duration>,
    /// Answer `slow_tool` with one `application/json` body at the end, so no response headers
    /// leave until the call is done.
    json_only: bool,
}

struct CaptureServer {
    options: ServerOptions,
    sessions: AtomicU64,
}

async fn start_server(options: ServerOptions) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let state = Arc::new(CaptureServer {
        options,
        sessions: AtomicU64::new(0),
    });
    let app = Router::new()
        .route("/mcp", post(on_post).get(on_get).delete(on_delete))
        .fallback(on_other)
        .with_state(state);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            log("mcp", "server.failed", json!(error.to_string()));
        }
    });
    let url = format!("http://{address}/mcp");
    log(
        "mcp",
        "server.listening",
        json!({
            "url": url,
            "progressEverySecs": options.progress_every.map(|every| every.as_secs()),
            "slowResponse": if options.json_only { "json" } else { "sse when accepted" },
        }),
    );
    Ok(url)
}

fn headers_json(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for name in headers.keys() {
        let values = headers
            .get_all(name)
            .iter()
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .collect::<Vec<_>>();
        let value = match values.as_slice() {
            [one] => json!(one),
            _ => json!(values),
        };
        map.insert(name.as_str().to_owned(), value);
    }
    Value::Object(map)
}

async fn on_post(
    State(server): State<Arc<CaptureServer>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let message = serde_json::from_slice::<Value>(&body)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&body)));
    log(
        "mcp",
        "http.post",
        json!({"headers": headers_json(&headers), "body": message}),
    );
    let accepts_sse = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    // Client responses and notifications get 202 with no body, as the transport specifies.
    let (Some(method), Some(id)) = (
        message.get("method").and_then(Value::as_str),
        message.get("id").cloned(),
    ) else {
        return empty(StatusCode::ACCEPTED);
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let session = format!(
                "capture-session-{}",
                server.sessions.fetch_add(1, Ordering::SeqCst) + 1
            );
            let version = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
            json_response(
                &id,
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "suru-broker-capture", "version": "0.0.0"},
                })),
                Some(&session),
            )
        }
        "ping" => json_response(&id, Ok(json!({})), None),
        "tools/list" => json_response(&id, Ok(json!({"tools": tools()})), None),
        "tools/call" => call_tool(server.options, id, &params, accepts_sse).await,
        other => json_response(
            &id,
            Err((-32601, format!("method not found: {other}"))),
            None,
        ),
    }
}

async fn on_get(headers: HeaderMap) -> Response {
    log(
        "mcp",
        "http.get",
        json!({"headers": headers_json(&headers)}),
    );
    let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
    response.headers_mut().insert(
        header::ALLOW,
        header::HeaderValue::from_static("POST, DELETE"),
    );
    response
}

async fn on_delete(headers: HeaderMap) -> Response {
    log(
        "mcp",
        "http.delete",
        json!({"headers": headers_json(&headers)}),
    );
    empty(StatusCode::OK)
}

async fn on_other(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    log(
        "mcp",
        "http.other",
        json!({"method": method.as_str(), "uri": uri.to_string(), "headers": headers_json(&headers)}),
    );
    empty(StatusCode::NOT_FOUND)
}

fn empty(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn json_response(
    id: &Value,
    result: Result<Value, (i64, String)>,
    session: Option<&str>,
) -> Response {
    let body = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    log(
        "mcp",
        "http.response",
        json!({"contentType": "application/json", "sessionId": session, "body": body}),
    );
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}

fn tools() -> Value {
    json!([
        {
            "name": "echo_context",
            "description": "Records the context of this call for a capture. Call it when asked to.",
            "inputSchema": {
                "type": "object",
                "properties": {"note": {"type": "string", "description": "Who is calling"}},
                "required": ["note"],
            },
        },
        {
            "name": "slow_tool",
            "description": "Waits for the given number of seconds, then returns. Call it when asked to.",
            "inputSchema": {
                "type": "object",
                "properties": {"seconds": {"type": "integer", "minimum": 1, "maximum": 3600}},
                "required": ["seconds"],
            },
        },
    ])
}

fn tool_text(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": false})
}

async fn call_tool(
    options: ServerOptions,
    id: Value,
    params: &Value,
    accepts_sse: bool,
) -> Response {
    let arguments = &params["arguments"];
    match params["name"].as_str().unwrap_or_default() {
        "echo_context" => {
            let meta_keys = params
                .get("_meta")
                .and_then(Value::as_object)
                .map(|meta| meta.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let text = format!(
                "echo_context recorded note {}; _meta keys: {meta_keys:?}",
                arguments["note"]
            );
            json_response(&id, Ok(tool_text(text)), None)
        }
        "slow_tool" => {
            let seconds = arguments["seconds"]
                .as_u64()
                .or_else(|| arguments["seconds"].as_f64().map(|seconds| seconds as u64))
                .unwrap_or(10)
                .clamp(1, 3600);
            let token = params
                .get("_meta")
                .and_then(|meta| meta.get("progressToken"))
                .cloned();
            if accepts_sse && !options.json_only {
                slow_over_sse(id, seconds, token, options.progress_every)
            } else {
                slow_as_json(id, seconds).await
            }
        }
        other => json_response(
            &id,
            Ok(
                json!({"content": [{"type": "text", "text": format!("no tool {other}")}], "isError": true}),
            ),
            None,
        ),
    }
}

fn sse_event(message: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {message}\n\n"))
}

/// Answers at once with an event stream, then sends progress on schedule and the result at the
/// end. The stream's receiver lives in the response body, so its sender sees the client go away.
fn slow_over_sse(
    id: Value,
    seconds: u64,
    token: Option<Value>,
    every: Option<Duration>,
) -> Response {
    let (sender, mut receiver) = mpsc::channel::<Bytes>(16);
    let call = id.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let total = Duration::from_secs(seconds);
        log(
            "mcp",
            "slow_tool.started",
            json!({"id": call, "seconds": seconds, "progressToken": token,
                   "progressEverySecs": every.map(|every| every.as_secs())}),
        );
        let mut sent = 0_u32;
        loop {
            let elapsed = started.elapsed();
            if elapsed >= total {
                break;
            }
            let wake = match (every, &token) {
                (Some(every), Some(_)) => (every * (sent + 1)).min(total),
                _ => total,
            };
            tokio::select! {
                () = tokio::time::sleep(wake.saturating_sub(elapsed)) => {}
                () = sender.closed() => {
                    log("mcp", "slow_tool.client_closed",
                        json!({"id": call, "afterSecs": started.elapsed().as_secs_f64()}));
                    return;
                }
            }
            if started.elapsed() >= total {
                break;
            }
            if let (Some(_), Some(token)) = (every, &token) {
                sent += 1;
                let elapsed = started.elapsed().as_secs();
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": elapsed,
                        "total": seconds,
                        "message": format!("{elapsed}s of {seconds}s"),
                    },
                });
                let queued = sender.send(sse_event(&note)).await.is_ok();
                log(
                    "mcp",
                    "sse.progress",
                    json!({"id": call, "queued": queued, "body": note}),
                );
                if !queued {
                    return;
                }
            }
        }
        let result = json!({
            "jsonrpc": "2.0",
            "id": call,
            "result": tool_text(format!("slow_tool finished after {seconds} seconds")),
        });
        let queued = sender.send(sse_event(&result)).await.is_ok();
        log(
            "mcp",
            "sse.result",
            json!({"queued": queued, "afterSecs": started.elapsed().as_secs_f64(), "body": result}),
        );
    });
    let stream = futures_util::stream::poll_fn(move |context| {
        receiver
            .poll_recv(context)
            .map(|item| item.map(Ok::<_, Infallible>))
    });
    log(
        "mcp",
        "http.response",
        json!({"contentType": "text/event-stream", "id": id}),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("static response parts are valid")
}

/// Holds the response back until the call is done, so no headers reach the client before then.
async fn slow_as_json(id: Value, seconds: u64) -> Response {
    struct Pending {
        id: Value,
        started: Instant,
        answered: bool,
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            if !self.answered {
                log(
                    "mcp",
                    "slow_tool.request_dropped",
                    json!({"id": self.id, "afterSecs": self.started.elapsed().as_secs_f64()}),
                );
            }
        }
    }
    let mut pending = Pending {
        id: id.clone(),
        started: Instant::now(),
        answered: false,
    };
    log(
        "mcp",
        "slow_tool.started",
        json!({"id": id, "seconds": seconds, "response": "json"}),
    );
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    pending.answered = true;
    json_response(
        &id,
        Ok(tool_text(format!(
            "slow_tool finished after {seconds} seconds"
        ))),
        None,
    )
}

// ------------------------------------------------------------------------------------------------
// Line-oriented child processes
// ------------------------------------------------------------------------------------------------

fn program(variable: &str, default: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

struct LineChild {
    source: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}

impl LineChild {
    fn spawn(source: &'static str, mut command: Command) -> anyhow::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| format!("spawn {source}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("child stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(source, "stderr", json!(line));
                }
            });
        }
        Ok(Self {
            source,
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    async fn send(&mut self, message: &Value) -> anyhow::Result<()> {
        log(self.source, "send", message.clone());
        let stdin = self.stdin.as_mut().context("stdin already closed")?;
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn next(&mut self, deadline: Instant) -> anyhow::Result<Value> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = tokio::time::timeout(remaining, self.lines.next_line())
                .await
                .map_err(|_| anyhow!("{} said nothing before the deadline", self.source))??
                .ok_or_else(|| anyhow!("{} closed its output", self.source))?;
            match serde_json::from_str(&line) {
                Ok(value) => return Ok(value),
                Err(_) => log(self.source, "stdout.text", json!(line)),
            }
        }
    }

    async fn finish(mut self) {
        drop(self.stdin.take());
        match tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await {
            Ok(status) => log(
                self.source,
                "exit",
                json!(status.map(|status| status.to_string()).unwrap_or_default()),
            ),
            Err(_) => {
                let _ = self.child.kill().await;
                log(self.source, "exit", json!("killed after 20 s"));
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Codex: one app-server per launch, driven over JSON-RPC as Suru's Codex Provider does
// ------------------------------------------------------------------------------------------------

const CODEX_TURN: Duration = Duration::from_secs(600);

struct AppServer {
    process: LineChild,
    next_id: i64,
}

impl AppServer {
    async fn launch(overrides: &[String]) -> anyhow::Result<Self> {
        let mut command = Command::new(program("SURU_CODEX_PATH", "codex"));
        command.arg("app-server");
        for value in overrides {
            command.arg("-c").arg(value);
        }
        log(
            "driver",
            "codex.launch",
            json!({"args": ["app-server"], "overrides": overrides}),
        );
        let mut server = Self {
            process: LineChild::spawn("codex", command)?,
            next_id: 0,
        };
        server
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name": "suru-capture", "title": "Suru capture", "version": "0"},
                    "capabilities": {"experimentalApi": false},
                }),
            )
            .await?
            .map_err(|error| anyhow!("initialize failed: {error}"))?;
        server
            .process
            .send(&json!({"jsonrpc": "2.0", "method": "initialized"}))
            .await?;
        Ok(server)
    }

    /// Sends a request and reads until its response, handling everything that arrives first.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Result<Value, Value>> {
        self.next_id += 1;
        let id = self.next_id;
        self.process
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let message = self.process.next(deadline).await?;
            if message.get("method").is_none() && message["id"] == json!(id) {
                log("codex", "response", message.clone());
                return Ok(match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                });
            }
            self.handle(message).await?;
        }
    }

    async fn handle(&mut self, message: Value) -> anyhow::Result<()> {
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        if method.ends_with("/delta") || method.contains("Delta") {
            return Ok(());
        }
        let Some(id) = message.get("id").cloned() else {
            log("codex", "notification", message);
            return Ok(());
        };
        log("codex", "server_request", message);
        let reply = match method.as_str() {
            // Refused as Suru's Codex transport refuses it today.
            "mcpServer/elicitation/request" => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32000, "message": "Suru does not support interactive request `mcpServer/elicitation/request`"},
            }),
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => json!({
                "jsonrpc": "2.0", "id": id, "result": {"decision": "decline"},
            }),
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("capture does not answer `{method}`")},
            }),
        };
        self.process.send(&reply).await
    }

    /// Starts a Turn and reads until that thread's `turn/completed`.
    async fn turn(&mut self, thread_id: &str, prompt: &str) -> anyhow::Result<()> {
        self.request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": prompt}]}),
        )
        .await?
        .map_err(|error| anyhow!("turn/start failed: {error}"))?;
        let deadline = Instant::now() + CODEX_TURN;
        loop {
            let message = self.process.next(deadline).await?;
            let done = message["method"] == "turn/completed"
                && message["params"]["threadId"] == json!(thread_id);
            self.handle(message).await?;
            if done {
                return Ok(());
            }
        }
    }
}

fn codex_server_entry(url: &str, token: &str, tool_timeout: f64, approve: bool) -> Value {
    let mut entry = json!({
        "url": url,
        "http_headers": {"Authorization": format!("Bearer {token}")},
        "tool_timeout_sec": tool_timeout,
    });
    if approve {
        entry["default_tools_approval_mode"] = json!("approve");
    }
    entry
}

fn codex_thread_params(
    cwd: &str,
    model: &str,
    reviewer: Option<&str>,
    server: Option<Value>,
) -> serde_json::Map<String, Value> {
    let mut config = json!({"model_reasoning_effort": "low"});
    if let Some(server) = server {
        config[format!("mcp_servers.{SERVER}")] = server;
    }
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(cwd));
    params.insert("model".into(), json!(model));
    params.insert("approvalPolicy".into(), json!("on-request"));
    params.insert("sandbox".into(), json!("workspace-write"));
    params.insert("config".into(), config);
    if let Some(reviewer) = reviewer {
        params.insert("approvalsReviewer".into(), json!(reviewer));
    }
    params
}

async fn codex(scenario: &str, options: &Options) -> anyhow::Result<()> {
    let url = start_server(options.server()?).await?;
    let workspace = tempfile::tempdir()?;
    let cwd = workspace
        .path()
        .to_str()
        .context("workspace path is not UTF-8")?
        .to_owned();
    let model = options.model("gpt-5.6-luna");
    let reviewer = options.get("reviewer");
    let echo_then_slow = |note: &str| {
        format!(
            "Call the {SERVER} MCP server's echo_context tool with note \"{note}\". Then call its \
             slow_tool tool with seconds 40. Do not retry either call. Then reply with one short \
             line giving what each call returned, quoting any error exactly."
        )
    };
    match scenario {
        "thread-config" => {
            let mut server = AppServer::launch(&[]).await?;
            let mut params = codex_thread_params(
                &cwd,
                model,
                reviewer,
                Some(codex_server_entry(&url, "capture-token-start", 20.0, true)),
            );
            params.insert("ephemeral".into(), json!(false));
            let started = server
                .request("thread/start", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/start failed: {error}"))?;
            let thread_id = started["thread"]["id"]
                .as_str()
                .context("thread id")?
                .to_owned();
            server
                .turn(&thread_id, &echo_then_slow("codex-start"))
                .await?;
            server.process.finish().await;

            let mut server = AppServer::launch(&[]).await?;
            let mut params = codex_thread_params(
                &cwd,
                model,
                reviewer,
                Some(codex_server_entry(&url, "capture-token-resume", 90.0, true)),
            );
            params.insert("threadId".into(), json!(thread_id));
            server
                .request("thread/resume", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/resume failed: {error}"))?;
            server
                .turn(&thread_id, &echo_then_slow("codex-resume"))
                .await?;
            server.process.finish().await;

            let mut server = AppServer::launch(&[]).await?;
            let mut params = codex_thread_params(&cwd, model, reviewer, None);
            params.insert("threadId".into(), json!(thread_id));
            server
                .request("thread/resume", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/resume failed: {error}"))?;
            server
                .turn(
                    &thread_id,
                    "If you have a tool named echo_context right now, call it with note \
                     \"codex-resume-bare\". If you do not, do not call anything and reply \
                     NO-TOOL. Reply in one short line.",
                )
                .await?;
            server.process.finish().await;
        }
        "launch-overrides" => {
            let overrides = [
                format!("mcp_servers.{SERVER}.url=\"{url}\""),
                format!(
                    "mcp_servers.{SERVER}.http_headers={{Authorization=\"Bearer capture-token-launch\"}}"
                ),
                format!("mcp_servers.{SERVER}.tool_timeout_sec=20.0"),
                format!("mcp_servers.{SERVER}.default_tools_approval_mode=\"approve\""),
            ];
            let mut server = AppServer::launch(&overrides).await?;
            let params = codex_thread_params(&cwd, model, reviewer, None);
            let started = server
                .request("thread/start", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/start failed: {error}"))?;
            let thread_id = started["thread"]["id"]
                .as_str()
                .context("thread id")?
                .to_owned();
            server
                .turn(&thread_id, &echo_then_slow("codex-launch"))
                .await?;
            server.process.finish().await;
        }
        "bearer-token" => {
            let mut server = AppServer::launch(&[]).await?;
            let mut entry = codex_server_entry(&url, "unused", 20.0, true);
            entry["bearer_token"] = json!("capture-token-inline");
            entry
                .as_object_mut()
                .expect("entry is an object")
                .remove("http_headers");
            let params = codex_thread_params(&cwd, model, reviewer, Some(entry));
            let outcome = server
                .request("thread/start", Value::Object(params))
                .await?;
            log(
                "driver",
                "codex.bearer_token",
                json!({"accepted": outcome.is_ok()}),
            );
            server.process.finish().await;
        }
        "no-approve" => {
            let mut server = AppServer::launch(&[]).await?;
            let params = codex_thread_params(
                &cwd,
                model,
                reviewer,
                Some(codex_server_entry(
                    &url,
                    "capture-token-no-approve",
                    20.0,
                    false,
                )),
            );
            let started = server
                .request("thread/start", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/start failed: {error}"))?;
            let thread_id = started["thread"]["id"]
                .as_str()
                .context("thread id")?
                .to_owned();
            server
                .turn(
                    &thread_id,
                    &format!(
                        "Call the {SERVER} MCP server's echo_context tool with note \
                         \"codex-no-approve\". Do not retry. Reply with one short line giving \
                         what it returned, quoting any error exactly."
                    ),
                )
                .await?;
            server.process.finish().await;
        }
        "subagent" => {
            let mut server = AppServer::launch(&[]).await?;
            let params = codex_thread_params(
                &cwd,
                model,
                reviewer,
                Some(codex_server_entry(&url, "capture-token-parent", 60.0, true)),
            );
            let started = server
                .request("thread/start", Value::Object(params))
                .await?
                .map_err(|error| anyhow!("thread/start failed: {error}"))?;
            let thread_id = started["thread"]["id"]
                .as_str()
                .context("thread id")?
                .to_owned();
            server
                .turn(
                    &thread_id,
                    &format!(
                        "Step 1: call the {SERVER} MCP server's echo_context tool with note \
                         \"codex-parent\". Step 2: spawn one sub-agent with spawn_agent whose \
                         task is exactly: 'Call the echo_context tool with note \"codex-child\", \
                         then reply done.' Step 3: wait for it with wait_agent. Then reply with \
                         one short line."
                    ),
                )
                .await?;
            server.process.finish().await;
        }
        other => bail!("unknown codex scenario `{other}`"),
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Claude: one stream-json process, permission prompts over stdio, as Suru's Claude Provider does
// ------------------------------------------------------------------------------------------------

async fn claude(scenario: &str, options: &Options) -> anyhow::Result<()> {
    let url = start_server(options.server()?).await?;
    let mut entry = json!({
        "type": "http",
        "url": url,
        "headers": {"Authorization": "Bearer capture-token-claude"},
    });
    if let Some(timeout) = options.number("timeout-ms")? {
        entry["timeout"] = json!(timeout);
    }
    let config = json!({"mcpServers": {SERVER: entry}});
    let (prompt, allowlist) = match scenario {
        "attribution" => (
            format!(
                "Do these steps in order. 1) Call the mcp__{SERVER}__echo_context tool with note \
                 \"claude-main\". 2) Use your Agent tool (also called Task) to launch one \
                 general-purpose subagent{} with exactly this prompt: 'Call the \
                 mcp__{SERVER}__echo_context tool with note \"claude-subagent\", then reply \
                 done.' 3) When it finishes, reply with one short line.",
                if options.get("background") == Some("1") {
                    " in the background (run_in_background: true)"
                } else {
                    " in the foreground (run_in_background: false)"
                }
            ),
            true,
        ),
        "approval" => (
            format!(
                "Call the mcp__{SERVER}__echo_context tool with note \"claude-approval\". Do not \
                 retry. Reply with one short line."
            ),
            options.get("allow") == Some("1"),
        ),
        "slow" => {
            let seconds = options.number("seconds")?.unwrap_or(420);
            (
                format!(
                    "Call the mcp__{SERVER}__slow_tool tool with seconds {seconds}. Do not retry \
                     it and do not call anything else. When it returns or fails, reply with one \
                     short line quoting exactly what it returned or the exact error text."
                ),
                true,
            )
        }
        other => bail!("unknown claude scenario `{other}`"),
    };
    let model = options.model("haiku");
    let mut args = vec![
        "--print".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--setting-sources".to_owned(),
        String::new(),
        "--strict-mcp-config".to_owned(),
        "--mcp-config".to_owned(),
        config.to_string(),
        "--permission-mode".to_owned(),
        "default".to_owned(),
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--no-session-persistence".to_owned(),
        "--model".to_owned(),
        model.to_owned(),
    ];
    if allowlist {
        args.push("--allowedTools".to_owned());
        args.push(format!("mcp__{SERVER}__*"));
    }
    let mut command = Command::new(program("SURU_CLAUDE_PATH", "claude"));
    command.args(&args);
    let removed = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| name.starts_with("CLAUDE"))
        .collect::<Vec<_>>();
    for name in &removed {
        command.env_remove(name);
    }
    for (name, value) in &options.env {
        command.env(name, value);
    }
    let workspace = tempfile::tempdir()?;
    command.current_dir(workspace.path());
    log(
        "driver",
        "claude.launch",
        json!({"args": args, "removedEnv": removed, "env": options.env}),
    );
    let mut process = LineChild::spawn("claude", command)?;
    process
        .send(&json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        }))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(1500);
    // A background Subagent outlives the Turn that spawned it, so a `result` ends the capture only
    // once no background task is listed; after that, a quiet spell ends it.
    let mut background_tasks = 0_usize;
    let mut results = 0_usize;
    loop {
        let settled = results > 0 && background_tasks == 0;
        let until = if settled {
            deadline.min(Instant::now() + Duration::from_secs(30))
        } else {
            deadline
        };
        let message = match process.next(until).await {
            Ok(message) => message,
            Err(_) if settled => break,
            Err(error) => return Err(error),
        };
        let kind = message["type"].as_str().unwrap_or_default().to_owned();
        if kind == "stream_event" {
            continue;
        }
        log("claude", "recv", message.clone());
        if kind == "system" && message["subtype"] == "background_tasks_changed" {
            background_tasks = message["tasks"].as_array().map_or(0, Vec::len);
        }
        if kind == "control_request" {
            let request = &message["request"];
            let response = if request["subtype"] == "can_use_tool" {
                json!({"behavior": "allow", "updatedInput": request["input"]})
            } else {
                json!({})
            };
            process
                .send(&json!({
                    "type": "control_response",
                    "response": {"subtype": "success", "request_id": message["request_id"], "response": response},
                }))
                .await?;
        }
        if kind == "result" {
            results += 1;
            if background_tasks == 0 {
                break;
            }
        }
    }
    process.finish().await;
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Copilot: one CLI process through the SDK, as Suru's Copilot Provider drives it
// ------------------------------------------------------------------------------------------------

/// Logs each permission request the CLI routes to the host and approves it once, as Suru's
/// handler does under allowAll.
struct LoggingApprover;

#[async_trait::async_trait]
impl PermissionHandler for LoggingApprover {
    async fn handle(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        log(
            "copilot",
            "permission.request",
            json!({
                "sessionId": serde_json::to_value(&session_id).unwrap_or_default(),
                "requestId": serde_json::to_value(&request_id).unwrap_or_default(),
                "data": serde_json::to_value(&data).unwrap_or_default(),
            }),
        );
        PermissionResult::approve_once()
    }
}

fn copilot_servers(
    url: &str,
    token: &str,
    timeout: Option<u64>,
) -> IndexMap<String, McpServerConfig> {
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
    let mut servers = IndexMap::new();
    servers.insert(
        SERVER.to_owned(),
        McpServerConfig::Http(McpHttpServerConfig {
            url: url.to_owned(),
            headers,
            timeout: timeout.map(|timeout| timeout as i64),
            ..Default::default()
        }),
    );
    servers
}

async fn copilot_client() -> anyhow::Result<Client> {
    let program = program("SURU_COPILOT_PATH", "copilot");
    Ok(Client::start(ClientOptions::new().with_program(CliProgram::Path(program))).await?)
}

async fn copilot_session(
    client: &Client,
    model: &str,
    workspace: &std::path::Path,
    servers: IndexMap<String, McpServerConfig>,
) -> anyhow::Result<Session> {
    let session = client
        .create_session(
            SessionConfig::default()
                .with_model(model)
                .with_working_directory(workspace)
                .with_streaming(true)
                .with_include_sub_agent_streaming_events(true)
                .with_mcp_servers(servers)
                .with_permission_handler(Arc::new(LoggingApprover)),
        )
        .await?;
    log(
        "copilot",
        "session.created",
        json!({"sessionId": serde_json::to_value(session.id()).unwrap_or_default()}),
    );
    Ok(session)
}

/// Sends one Prompt and logs the Session's events until it is idle with no Subagent working.
async fn copilot_turn(session: &Session, label: &str, prompt: &str) -> anyhow::Result<()> {
    let mut events = session.subscribe();
    log(
        "copilot",
        "send",
        json!({"session": label, "prompt": prompt}),
    );
    session.send(MessageOptions::new(prompt)).await?;
    let mut working = 0_i32;
    let deadline = Instant::now() + Duration::from_secs(1500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, events.recv())
            .await
            .map_err(|_| anyhow!("Copilot Session {label} did not settle before the deadline"))?
            .map_err(|error| anyhow!("Copilot event stream ended: {error:?}"))?;
        if event.event_type.contains("delta") {
            continue;
        }
        log(
            "copilot",
            "event",
            json!({"session": label, "event": serde_json::to_value(&event)?}),
        );
        match event.event_type.as_str() {
            "subagent.started" => working += 1,
            "subagent.completed" | "subagent.failed" => working -= 1,
            "session.idle" if working <= 0 => return Ok(()),
            _ => {}
        }
    }
}

async fn copilot(scenario: &str, options: &Options) -> anyhow::Result<()> {
    let url = start_server(options.server()?).await?;
    let workspace = tempfile::tempdir()?;
    let model = options.model("gpt-5-mini");
    let timeout = options.number("timeout-ms")?;
    let echo = |note: &str| {
        format!(
            "Call the {SERVER} MCP server's echo_context tool with note \"{note}\". Do not retry. \
             Reply with one short line."
        )
    };
    match scenario {
        "attribution" => {
            let client = copilot_client().await?;
            let session = copilot_session(
                &client,
                model,
                workspace.path(),
                copilot_servers(&url, "capture-token-copilot", timeout),
            )
            .await?;
            copilot_turn(
                &session,
                "main",
                &format!(
                    "Do these steps in order. 1) Call the {SERVER} MCP server's echo_context tool \
                     with note \"copilot-main\". 2) Use the task tool to start one \
                     general-purpose agent with exactly this prompt: 'Call the {SERVER} MCP \
                     server's echo_context tool with note \"copilot-subagent\", then reply \
                     done.' and wait for it to finish. 3) Reply with one short line."
                ),
            )
            .await?;
            session.disconnect().await?;
            client.stop().await.map_err(|error| anyhow!("{error:?}"))?;
        }
        "two-sessions" => {
            let client = copilot_client().await?;
            let first = copilot_session(
                &client,
                model,
                workspace.path(),
                copilot_servers(&url, "capture-token-session-a", timeout),
            )
            .await?;
            let second = copilot_session(
                &client,
                model,
                workspace.path(),
                copilot_servers(&url, "capture-token-session-b", timeout),
            )
            .await?;
            copilot_turn(&first, "a", &echo("copilot-session-a")).await?;
            copilot_turn(&second, "b", &echo("copilot-session-b")).await?;
            copilot_turn(&first, "a", &echo("copilot-session-a-again")).await?;
            first.disconnect().await?;
            second.disconnect().await?;
            client.stop().await.map_err(|error| anyhow!("{error:?}"))?;
        }
        "resume" => {
            let client = copilot_client().await?;
            let session = copilot_session(
                &client,
                model,
                workspace.path(),
                copilot_servers(&url, "capture-token-create", timeout),
            )
            .await?;
            let id = session.id().clone();
            copilot_turn(&session, "create", &echo("copilot-create")).await?;
            session.disconnect().await?;
            client.stop().await.map_err(|error| anyhow!("{error:?}"))?;

            let client = copilot_client().await?;
            let session = client
                .resume_session(
                    ResumeSessionConfig::new(id)
                        .with_model(model)
                        .with_working_directory(workspace.path())
                        .with_streaming(true)
                        .with_include_sub_agent_streaming_events(true)
                        .with_mcp_servers(copilot_servers(&url, "capture-token-resume", timeout))
                        .with_permission_handler(Arc::new(LoggingApprover)),
                )
                .await?;
            copilot_turn(&session, "resume", &echo("copilot-resume")).await?;
            session.disconnect().await?;
            client.stop().await.map_err(|error| anyhow!("{error:?}"))?;
        }
        "slow" => {
            let seconds = options.number("seconds")?.unwrap_or(90);
            let client = copilot_client().await?;
            let session = copilot_session(
                &client,
                model,
                workspace.path(),
                copilot_servers(&url, "capture-token-slow", timeout),
            )
            .await?;
            copilot_turn(
                &session,
                "slow",
                &format!(
                    "Call the {SERVER} MCP server's slow_tool tool with seconds {seconds}. Do not \
                     retry it and do not call anything else. When it returns or fails, reply with \
                     one short line quoting exactly what it returned or the exact error text."
                ),
            )
            .await?;
            session.disconnect().await?;
            client.stop().await.map_err(|error| anyhow!("{error:?}"))?;
        }
        other => bail!("unknown copilot scenario `{other}`"),
    }
    Ok(())
}
