# Claude's HTTP MCP path under long calls (#408)

Captured 2026-09-27 against Claude Code **2.1.283**: `/usr/bin/claude`, a
wrapper that sets `DISABLE_UPDATES=1` and runs `/opt/claude-code/bin/claude`,
signed in with a claude.ai account. The Model was `haiku`
(`claude-haiku-4-5-20251001`). `examples/broker_harness_capture.rs claude
<scenario>` launched one stream-json process per run, as Suru's Claude Provider
does, from an empty working directory:

```
claude --print --input-format stream-json --output-format stream-json --verbose
  --setting-sources "" --strict-mcp-config --mcp-config '<entry below>'
  --permission-mode default --permission-prompt-tool stdio
  --no-session-persistence --model haiku --allowedTools 'mcp__capture__*'
```

The `--mcp-config` entry, with `timeout` present only where the table says so:

```json
{"mcpServers": {"capture": {"type": "http", "url": "http://127.0.0.1:<port>/mcp",
  "headers": {"Authorization": "Bearer capture-token-claude"}, "timeout": 900000}}}
```

The driver removed every inherited `CLAUDE*` variable (`CLAUDECODE`,
`CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_EFFORT`, and others),
so the capture did not inherit the session it was run from. This was not needed
to start the CLI: a nested `claude -p` with the variables left in place ran
normally. The endpoint was the example's loopback streamable-HTTP server. Its
`slow_tool` either answers at once with a `text/event-stream` and sends
`notifications/progress` against the call's `progressToken` on a schedule, or
holds a single `application/json` body back until it is done. Every call carried
a `progressToken`.

## Findings

| Run | `timeout` | Environment | Endpoint's answer | Call | Outcome |
| --- | --- | --- | --- | --- | --- |
| 1 | none | none | JSON at the end: no response headers until then | 90 s | Aborted at 60.0 s: `The operation timed out.` The endpoint saw the request dropped at 60.01 s. |
| 2 | 900000 | none | JSON at the end | 90 s | Completed at 90 s. |
| 3 | none | none | event stream at once, progress every 30 s | 420 s | Completed at 420 s after 13 progress notifications. Nothing happened at 60 s or at 300 s. |
| 4 | none | none | event stream at once, silent | 330 s | Aborted at 300.0 s (message below). |
| 5 | 600000 | none | event stream at once, silent | 330 s | Completed at 330 s. |
| 6 | 120000 | none | event stream at once, progress every 30 s | 180 s | Aborted at 120.1 s: `MCP server "capture" tool "slow_tool" timed out after 120s`. Then `notifications/cancelled` with `{"reason": "SdkError: Request timed out", "requestId": 2}`. |
| 7 | none | `MCP_TOOL_TIMEOUT=90000` | event stream at once, progress every 30 s | 150 s | Aborted at 90.0 s: `… timed out after 90s`. Then `notifications/cancelled` as in run 6. |

Run 4's error, as the Model received it:

> MCP server "capture" tool "slow_tool" sent no response or progress for 300s;
> aborting. If this server is configured in your MCP settings, set a per-server
> "timeout" (ms) to allow longer silent runs for just this server; otherwise set
> CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT (ms) globally (0 disables).

Three separate timers explain every row. Each agrees with the minified source in
the 2.1.283 binary, whose schema describes the per-server key as "Per-server
tool-call timeout in milliseconds. Overrides the MCP_TOOL_TIMEOUT environment
variable for this server. Hard wall-clock limit per call; progress notifications
do not extend it. Values below 1000ms are ignored."

1. **Request timer:** runs until the response headers arrive. It is 60 s, or
   the per-server `timeout` (else `MCP_TOOL_TIMEOUT`) when that is larger
   (runs 1 and 2). An answer that opens an event stream at once clears it, so
   this is the 60 s ADR 0034 expected, but it is not a limit on the call.
2. **Idle timer:** 300 s over HTTP since the last response bytes or progress.
   Progress resets it (run 3), and the per-server `timeout` raises it to that
   value when larger (run 5). `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` sets it for
   every server; stdio servers get 1800 s.
3. **Call timer:** the per-server `timeout`, else `MCP_TOOL_TIMEOUT`, as a hard
   wall-clock limit that progress does not extend (runs 6 and 7). With neither
   set, the binary's default is 10⁸ ms, about 28 hours.

### Approval

No run with `--allowedTools 'mcp__capture__*'` produced a `control_request`:
the seven above, and the two in `0408-subagent-mcp-attribution.md`, whose
Subagents' calls were allowed as well. Without the allowlist, the same call
raised one:

```json
{"type": "control_request", "request": {"subtype": "can_use_tool",
  "tool_name": "mcp__capture__echo_context", "display_name": "Echo Context",
  "input": {"note": "claude-approval"}, "mcp_server": {"name": "capture", "source": "dynamic"},
  "permission_suggestions": [{"type": "addRules", "behavior": "allow",
    "destination": "localSettings", "rules": [{"toolName": "mcp__capture__echo_context"}]}],
  "tool_use_id": "…"}}
```

### What else the endpoint saw

- **Discovery probe:** before `initialize`, Claude posted `server/discover`
  (id `server-discover-probe-1`). The endpoint answered `-32601`, and Claude
  fell back to `initialize` at protocol `2025-11-25`. It then opened the
  optional `GET` stream; a 405 was accepted. The Broker itself, served by
  rmcp, answers the probe, and Claude speaks `2026-07-28` to it instead. These
  timers were not measured at that revision, but a 335 s `wait_subagents` call
  to the Broker survived at it (`0421-broker-smoke.md`).
- **Server status:** `system` `init` listed the server as
  `{"name": "capture", "status": "connected", "source": "dynamic"}`.
- **Deferred Tools:** in 2.1.283 the server's Tools sit behind `ToolSearch`.
  Before its first use of a Tool, the Model called `ToolSearch` with
  `select:mcp__capture__echo_context`. A native `ToolSearch` call will
  therefore precede each first use of a Broker Tool. #411's projection guard
  must not mistake it for a Broker call, and #414's instruction note should
  name the Broker's Tools so the Agent selects them.

## Decision

The Broker relies on answering every `tools/call` with an event stream at once,
and on sending `notifications/progress` against the call's `progressToken`
every 30 s while it waits. The request timer never runs, and the 300 s idle
window never closes (run 3: 420 s under the default configuration).

The `--mcp-config` entry carries `"timeout": 900000`. It is a hard limit per
call, so it must stand above the 600 s ceiling of `wait_subagents`, and 900 s
does. It also raises the idle window and the request timer to 900 s, so a
stretch with no progress, or a plain JSON answer, still gets through
(runs 2 and 5). `--allowedTools 'mcp__suru__*'` keeps Broker calls from raising
Approvals, Subagents' calls included.

**Fallback:** if a later Claude drops the per-server key, Suru sets
`MCP_TOOL_TIMEOUT` and `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` on the Session's own
process (run 7 shows the first taking effect). Claude runs one process per
Session, so these reach only that Session.
