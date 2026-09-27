# Per-Subagent signals on MCP calls (#408)

Captured 2026-09-27 with `examples/broker_harness_capture.rs`. In each run the
main agent called the loopback MCP endpoint's `echo_context` itself, then had
one native Subagent call it. The endpoint recorded every header and the whole
JSON-RPC request. Each harness was driven the way Suru drives it:

- **Claude Code 2.1.283**, Model `haiku` (`claude-haiku-4-5-20251001`). One
  stream-json process with `--mcp-config` naming an HTTP server `capture`
  (`Authorization: Bearer capture-token-claude`), `--allowedTools
  'mcp__capture__*'`, `--permission-prompt-tool stdio`. The Subagent was
  launched through the `Agent` tool, once in the foreground and once with
  `run_in_background: true` (`claude attribution [--background 1]`).
- **GitHub Copilot CLI 1.0.88**, through `github-copilot-sdk`
  1.0.15-preview.3, Model `gpt-5-mini`. One Session whose `mcp_servers` named
  `capture` over HTTP (`Authorization: Bearer capture-token-copilot`), with a
  permission handler that logs each request and approves it. The Subagent was a
  `task` agent (`agent_type: general-purpose`, `mode: sync`), also on
  `gpt-5-mini` (`copilot attribution`).
- **Codex CLI 0.157.1**, Model `gpt-5.6-luna`, for comparison. The server came
  through the per-thread `config` map (`0408-codex-per-thread-mcp-config.md`),
  and the Subagent was spawned with `spawn_agent` (`codex subagent --reviewer
  user`).

## Findings

### Claude: no signal in headers; `_meta` names the calling `tool_use`

The main agent's call and the Subagent's call carried the same headers,
byte for byte apart from `content-length`, over the same MCP session:

```
authorization: Bearer capture-token-claude
mcp-session-id: capture-session-1
mcp-protocol-version: 2025-11-25
user-agent: claude-code/2.1.283 (sdk-cli)
```

Each call's `_meta` held two keys:

```json
{"claudecode/toolUseId": "toolu_01X4Ei…", "progressToken": 3}
```

`claudecode/toolUseId` is the id of the `tool_use` block the calling agent
emitted, and stream-json prints that block before the call arrives. The
Subagent's block comes in an `assistant` message whose `parent_tool_use_id`
names the `Agent` call that spawned it:

| Run | Caller | stream-json `tool_use` (`parent_tool_use_id`) | stdout at | MCP call at | `_meta["claudecode/toolUseId"]` |
| --- | --- | --- | --- | --- | --- |
| foreground | main agent | `toolu_01UHKT…` (none) | 4.901 s | 4.906 s | `toolu_01UHKT…` |
| foreground | Subagent | `toolu_01X4Ei…` (`toolu_0197m3…`, the `Agent` call) | 9.337 s | 9.338 s | `toolu_01X4Ei…` |
| background | main agent | `toolu_01SEZT…` (none) | 5.378 s | 5.381 s | `toolu_01SEZT…` |
| background | Subagent | `toolu_0122Vh…` (`toolu_019PcV…`, the `Agent` call) | 9.861 s | 9.866 s | `toolu_0122Vh…` |

In the background run, the parent's Turn had already ended (`result` at
8.377 s) when the Subagent's call arrived. `system` `task_started` names the
same `Agent` call as `tool_use_id`, beside the `task_id`. The Subagent used the
parent's MCP connection; no second `initialize` was seen. Neither call raised a
`can_use_tool` request, so the allowlist covers Subagents.

### Copilot: no signal in headers or `_meta`

The main agent's call and the `task` agent's call carried the same headers over
the same MCP session:

```
authorization: Bearer capture-token-copilot
mcp-session-id: capture-session-1
mcp-protocol-version: 2025-11-25
user-agent: copilot-cli
```

Each call's `_meta` held only a counter, `{"progressToken": 1}` and then
`{"progressToken": 2}`. The Subagent is visible only on the SDK's event stream:

- `tool.execution_start` for the Subagent's call carries its instance id in the
  envelope's `agentId` (`6d14c337-…`), the `toolCallId` (`call_7JUr…`), and
  `parentToolCallId` naming the `task` call.
- The permission request Suru's handler receives names the root Session's
  `sessionId`, `kind: "mcp"`, `serverName: "capture"`, `toolName:
  "capture-echo_context"` and the same `toolCallId`, but no agent. The
  `permission.requested` event that mirrors it carries the `agentId` in its
  envelope.
- The handler was asked 2 ms before the MCP request, and the request itself
  carries nothing that names `call_7JUr…`.

Joining a call to a Subagent would therefore have to match on timing and
arguments.

### Codex, for comparison: `_meta.threadId` names the calling thread

| Caller | `_meta.threadId` | `_meta.sessionId` | `x-codex-turn-metadata` extras | MCP session |
| --- | --- | --- | --- | --- |
| parent | parent thread `01a0e25a-60f7…` | parent thread | none | `capture-session-1` |
| native child | child thread `01a0e25a-8213…` | parent thread | `parent_thread_id` (parent thread), `subagent_kind: "thread_spawn"`, `thread_source: "subagent"`, the child's `turn_id` | `capture-session-2` |

The child thread id is the `receiverThreadIds[0]` of the `collabAgentToolCall`
item (`tool: "spawnAgent"`) that Suru's Codex projection already reads. Both
calls carried the parent's token. Every call's `_meta` also had `callId`,
`itemId`, `windowId` and `progressToken`.

## Decision

- **Codex:** the Broker attributes a call to the Session of the thread named by
  `_meta.threadId`, as ADR 0034 and ADR 0035 expect. `sessionId` names the root
  and must not be used for this. Fallback: the token's Session, when `threadId`
  names no Session the Broker knows.
- **Claude: attribution can improve to the exact Subagent.** A call's
  `_meta["claudecode/toolUseId"]` names a `tool_use` that Suru's Claude
  projection has already read with its `parent_tool_use_id`, and so already
  places in a native Subagent's Session or the root's. This is a join on
  identity, not the timing-and-arguments correlation ADR 0035 refused. The
  `tool_use` reached stdout 1–5 ms before its call in every run, but the two
  arrive on different channels, so the Broker waits, briefly and with a bound,
  for the projection to have read the id. Fallback: the token's Session, when
  the key is absent or its id is not seen within the bound. The key is
  vendor-prefixed and undocumented, so its absence is expected someday.
- **Copilot: attribution stays at the token's Session.** Nothing on the call
  names the Subagent, and pairing it with the permission request or
  `tool.execution_start` would be the correlation ADR 0035 refused. Fallback:
  none needed; the token's Session is the path.
