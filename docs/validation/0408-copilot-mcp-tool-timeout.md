# Copilot's MCP tool-call timeout (#408)

Captured 2026-09-27 against GitHub Copilot CLI **1.0.88** (`/usr/bin/copilot`,
signed in), through the pinned `github-copilot-sdk` **1.0.15-preview.3**, with
Model `gpt-5-mini`. `examples/broker_harness_capture.rs copilot slow` started a
CLI through the SDK, as Suru's Copilot Provider does, and created one Session:

```rust
SessionConfig::default()
    .with_model("gpt-5-mini")
    .with_working_directory("/tmp/workspace")
    .with_streaming(true)
    .with_include_sub_agent_streaming_events(true)
    .with_mcp_servers({"capture": McpServerConfig::Http(McpHttpServerConfig {
        url: "http://127.0.0.1:<port>/mcp",
        headers: {"Authorization": "Bearer capture-token-slow"},
        timeout: <ms, where the table gives one>,
        tools: None, // every Tool
    })})
    .with_permission_handler(/* logs each request, approves it once */)
```

It then asked the Model to call `slow_tool` once with the given `seconds`. The
endpoint answered each call at once with a `text/event-stream`. It either
stayed silent until the result, or sent `notifications/progress` against the
call's `progressToken` on the given schedule. Every call carried
`_meta: {"progressToken": 1}`. The SDK's per-server `timeout` is documented only
as "Optional timeout in milliseconds for tool calls to this server", and
`copilot mcp add --help` documents `--timeout <ms>` with no default.

## Findings

| Run | `timeout` | Progress | Call | Outcome |
| --- | --- | --- | --- | --- |
| 1 | none | silent | 90 s | Completed. |
| 2 | none | silent | 660 s | Aborted at 180.3 s (below). |
| 3 | none | every 30 s | 150 s | Completed. |
| 4 | none | every 30 s | 240 s | Completed, past the 180 s of run 2. |
| 5 | 30000 | silent | 90 s | Aborted at 30.1 s, as in run 2. |
| 6 | 30000 | every 10 s | 60 s | Completed, past its own 30 s. |
| 7 | 300000 | silent | 150 s | Completed. |
| 8 | 600000 | silent | 240 s | Completed, past the default's 180 s. |
| 9 | 900000 | every 30 s | 660 s | Completed after 21 progress notifications: the Broker's configuration, over a call longer than its longest wait. |

Runs 2 and 5 ended the same way. Copilot posted
`{"method": "notifications/cancelled", "params": {"reason": "request timeout", "requestId": 3}}`
and closed the call's stream. `tool.execution_complete` then reported
`success: false` with
`"error": {"code": "failure", "message": "MCP server 'capture': McpError: MCP error -32001: Request timed out"}`
and telemetry `failure_category: "timeout"`, `failure_stage: "invoke"`. The
Model was handed the error text.

So Copilot's default is not "no timeout": **a call that goes 180 s without a
response or progress is cancelled.** Each progress notification resets that
deadline (runs 3 and 4). The per-server `timeout` replaces the 180 s, whether
shorter (run 5) or longer (run 8), and progress resets it the same way (run 6).
Each progress notification also reached the SDK as an ephemeral
`tool.execution_progress` event, e.g. `{"progressMessage": "30.0/150.0 (20%):
30s of 150s", "toolCallId": "…"}`.

### How the permission handler is asked

In every Copilot run (these, and `0408-subagent-mcp-attribution.md`), the SDK
asked the host's permission handler once per MCP call before sending it, and
asked for a `task` Subagent's calls too. Each request named the root Session's
id. The `data` it received, trimmed:

```json
{"kind": "mcp", "agentMode": "interactive", "permissionMode": "manual",
 "toolCallId": "call_…",
 "permissionRequest": {"kind": "mcp", "serverName": "capture",
   "toolName": "capture-echo_context", "toolTitle": "echo_context",
   "args": {"note": "copilot-main"}, "readOnly": false, "toolCallId": "call_…"},
 "promptRequest": {"kind": "mcp", "serverName": "capture", "toolName": "echo_context", "…": "…"}}
```

An approve-once answer produced `permission.completed` with
`{"result": {"kind": "approved"}}`, and the call went out about 2 ms later.
Suru's handler reads `permissionRequest`. Under the ask posture it would raise
an Approval with the subject `MCP capture/capture-echo_context`; under allowAll
it approves without asking.

### What else the endpoint saw

- **Lazy connection:** the MCP connection opened at a Session's first
  `session.send`, not at `create_session`. The CLI probed `server/discover`
  first, then `initialize` at protocol `2025-11-25` with `clientInfo`
  `copilot-cli` 1.0.88, then opened the optional `GET` stream (405 accepted).
- **Per-Session connections:** two Sessions created on one CLI process, naming
  the same server with different tokens, each opened a connection of their own.
  Each call carried its own Session's token, and a later call on the first
  Session still carried the first token (`copilot two-sessions`).
- **Resume:** `resume_session` on a new CLI process opened a new connection
  carrying the `mcp_servers` headers given to the resume, not those given at
  create (`copilot resume`).

## Decision

The Broker relies on the per-server `timeout` in `mcp_servers.suru` on
`create_session` and every `resume_session`, set to 900000, together with a
progress notification every 30 s while a call waits. Progress keeps resetting
the deadline, and the `timeout` gives any stretch without progress 900 s rather
than 180 s, which is above the 600 s ceiling of `wait_subagents`.

Suru's own permission handler approves, before any posture check, a request
whose `permissionRequest` has `kind: "mcp"` and `serverName: "suru"`. Copilot
asks it for every Broker call, a Subagent's included.

**Fallback:** progress alone carries a call past the 180 s default (run 4), so
a Broker call survives while progress flows even if a later CLI ignored the
per-server `timeout`.
