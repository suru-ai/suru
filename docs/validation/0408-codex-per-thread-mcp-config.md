# Codex MCP server through the per-thread config map (#408)

Captured 2026-09-27 against Codex CLI **0.157.1** (`/usr/bin/codex app-server`,
signed in with a ChatGPT account). The Model was `gpt-5.6-luna` at
`model_reasoning_effort = "low"`. `examples/broker_harness_capture.rs codex
<scenario>` stood in for Suru's Codex Provider: one `codex app-server` per
launch, `initialize` with `experimentalApi: false`, and a fresh app-server for
each resume. The MCP server was the example's loopback streamable-HTTP endpoint,
named `capture`. Its Tools carry no annotations, as a worst case for approval.
Source references are to the Codex checkout at `8f195c93`.

## Configuration

`thread/start` for run 1:

```json
{
  "cwd": "/tmp/workspace",
  "model": "gpt-5.6-luna",
  "approvalPolicy": "on-request",
  "sandbox": "workspace-write",
  "ephemeral": false,
  "config": {
    "model_reasoning_effort": "low",
    "mcp_servers.capture": {
      "url": "http://127.0.0.1:<port>/mcp",
      "http_headers": {"Authorization": "Bearer capture-token-start"},
      "tool_timeout_sec": 20.0,
      "default_tools_approval_mode": "approve"
    }
  }
}
```

Each `config` key is a dotted path, applied the way a `-c` override is
(`config/src/overrides.rs`), so `mcp_servers.capture` names one server table.

- **Run 2:** the `thread/resume` sent the same object plus `threadId`, with
  token `capture-token-resume` and `tool_timeout_sec: 90.0`, to a new
  app-server.
- **Run 4:** the launch-time comparison started `codex app-server` with these
  overrides, and a `thread/start` whose `config` carried only the reasoning
  effort:

  ```
  -c mcp_servers.capture.url="http://127.0.0.1:<port>/mcp"
  -c mcp_servers.capture.http_headers={Authorization="Bearer capture-token-launch"}
  -c mcp_servers.capture.tool_timeout_sec=20.0
  -c mcp_servers.capture.default_tools_approval_mode="approve"
  ```

- **Prompts:** runs 1, 2 and 4 asked the Model to call `echo_context`, then
  `slow_tool` with `seconds: 40`. The other runs asked for `echo_context` only.

## Findings

| Run | Seam | Server entry | Observed |
| --- | --- | --- | --- |
| 1 | `thread/start` `config` | token `…-start`, `tool_timeout_sec` 20, approve | Codex connected during `thread/start`, before answering it; `initialize` and `tools/list` carried `Authorization: Bearer capture-token-start`. `echo_context` ran without asking. `slow_tool` failed at 20.0 s: `timed out awaiting tools/call after 20s`. |
| 2 | `thread/resume` `config`, new app-server | token `…-resume`, 90, approve | A new MCP session carrying the new token. `slow_tool` completed after 40 s, with one progress notification at 30 s. |
| 3 | `thread/resume` without `mcp_servers`, new app-server | none | No connection at all. The Model answered `NO-TOOL`: the server is not stored with the thread. |
| 4 | launch-time `-c` | token `…-launch`, 20, approve | The same as run 1 in every respect. |
| 4b | as 4, progress every 5 s | as 4 | Four progress notifications went out, and the call still failed at 20.06 s. Progress does not extend `tool_timeout_sec`. |
| 5 | `thread/start` `config`, `bearer_token` in place of `http_headers` | inline token | Refused: `-32600 failed to load configuration: bearer_token is not supported for streamable_http in mcp_servers.capture`. |
| 6 | `thread/start` `config` without `default_tools_approval_mode`; this machine's `approvals_reviewer = "auto_review"` | | No request reached the client. Codex's automatic reviewer ran for 7.3 s, emitted `guardianWarning` ("Automatic approval review approved (risk: medium, authorization: high) …"), then made the call. |
| 7 | as 6, with `approvalsReviewer: "user"` on `thread/start` | | `mcpServer/elicitation/request` (below). The capture answered with the error Suru's transport sends today, and Codex failed the call as `user rejected MCP tool call`. |
| 8 | as 1, with `approvalsReviewer: "user"` and a native Subagent | token `…-parent`, 60, approve | Neither the parent's call nor the child's asked. |

Run 7's request, trimmed:

```json
{"method": "mcpServer/elicitation/request", "params": {
  "serverName": "capture", "threadId": "…", "turnId": "…", "mode": "form",
  "message": "Allow the capture MCP server to run tool \"echo_context\"?",
  "requestedSchema": {"type": "object", "properties": {}},
  "_meta": {"codex_approval_kind": "mcp_tool_call", "persist": ["session", "always"],
            "tool_params": {"note": "codex-no-approve"}, "…": "…"}}}
```

### What else the endpoint saw

- **Client and transport:** the client was `codex-mcp-client/0.157.1`, speaking
  protocol `2025-06-18` with `Accept: text/event-stream, application/json`.
  After `initialize` it opened the optional `GET` stream; the endpoint answered
  405 and Codex went on. When the app-server shut down it sent `DELETE` with its
  `mcp-session-id`.
- **One MCP session per thread:** a native Subagent's thread opened its own
  (run 8), sending the parent's headers, so a child thread inherits its
  parent's per-thread `mcp_servers`.
- **Startup status:** `mcpServer/startupStatus/updated` notifications report the
  server `starting`, then `ready`, and name the thread.
- **Call `_meta`:** each `tools/call` carried `callId`, `itemId`, `progressToken`,
  `sessionId`, `threadId`, `windowId` and `x-codex-turn-metadata` (see
  `0408-subagent-mcp-attribution.md`).
- **Progress:** Codex forwarded no progress notification to the app-server
  client; no progress method appeared on the JSON-RPC stream.
- **Timeouts:** when `tool_timeout_sec` expired, Codex neither sent
  `notifications/cancelled` nor closed the call's event stream. The stream stayed
  open until the app-server exited. The source's default tool timeout is 300 s
  (`DEFAULT_TOOL_TIMEOUT`, `codex-mcp/src/rmcp_client.rs`).

## Decision

The Broker relies on the per-thread `config` map on `thread/start` and on every
`thread/resume`, with one entry, `mcp_servers.suru`:

- `url`: the Broker's loopback endpoint.
- `http_headers`: `{"Authorization": "Bearer <token>"}`. Never `bearer_token`,
  which Codex refuses for streamable HTTP.
- `tool_timeout_sec`: 900. It is a hard limit per call that progress does not
  extend, so it must stand above the Broker's longest call, the 600 s ceiling of
  `wait_subagents`.
- `default_tools_approval_mode: "approve"`. This is mandatory. Without it, a
  Broker call either waits on Codex's automatic reviewer, which may refuse it,
  or raises an elicitation that Suru's transport rejects, failing the call.

A resume without the entry has no server, so the entry goes on every
`thread/resume` Suru sends, and the re-minted token with it. Nothing about the
server survives in the thread.

Codex does not cancel a call it has timed out. The Broker therefore bounds each
call by its own arguments rather than waiting for the client to go away.

**Fallback:** launch-time `-c mcp_servers.suru.…` overrides on `codex
app-server` behave identically. Suru launches one app-server per Session and
relaunches it for each resume, so the overrides would carry the same per-Session
token if a later Codex stopped honoring `mcp_servers` in the per-thread map.
