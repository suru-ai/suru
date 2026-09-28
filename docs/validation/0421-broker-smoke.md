# The Broker smoke with the installed CLIs (#421)

Last passed 2026-09-28 against Claude Code **2.1.283** (`/usr/bin/claude`, a
wrapper that sets `DISABLE_UPDATES=1` and runs `/opt/claude-code/bin/claude`,
signed in with a claude.ai account) and Codex CLI **0.157.1** (`/usr/bin/codex`,
signed in with a ChatGPT account). The delegating Agent ran Claude's `haiku`
(`claude-haiku-4-5-20251001`). The Subagent ran the Model the Agent chose from
`list_providers`: Codex's catalog default, `gpt-6-astra`, at that Model's default
effort, `medium`.

`tests/broker_smoke.rs` hosts a real Server with the real `ClaudeRuntime` and
`CodexRuntime` and drives it only through the client protocol. A Claude
Session's Agent spawns a Codex Subagent through the Broker. The Subagent's Turn
settles, and its Subagent Report wakes the Claude Agent into a Continuation,
where it answers.

## Running it

```
SURU_BROKER_SMOKE=1 cargo nextest run --test broker_smoke --run-ignored ignored-only --no-capture
```

`cargo test --test broker_smoke -- --ignored --nocapture` runs it too. It is gated
twice:

- **Ignore attribute:** `#[ignore = "set SURU_BROKER_SMOKE=1 to use the installed,
  signed-in Claude and Codex binaries"]`. `cargo nextest run` skips it
  (`0 tests run: 0 passed, 1 skipped` for this binary). `cargo nextest list
  --message-format json` reports it `ignored: true`, and it is listed only with
  `--run-ignored ignored-only` or `all`.
- **Runtime check:** without `SURU_BROKER_SMOKE=1`, an ignored-only run prints
  `skipping: set SURU_BROKER_SMOKE=1 to opt in` and returns.

Once opted in, the smoke refreshes the Model Catalog and skips, printing the
reason, when Suru finds either CLI not installed or not signed in. It fails if a
Provider's catalog fails or reports an incompatible version.

`SURU_CLAUDE_PATH` and `SURU_CODEX_PATH` name other binaries, as they do for
Suru itself. `SURU_LOG` (for example `warn,suru=info,rmcp=debug`) writes the
Server's tracing to stderr. The smoke prints both CLIs' `--version` first, then
the elapsed time at each milestone.

Using the real CLIs leaves their own records behind: Claude's under
`~/.claude/projects/` and Codex's rollouts under `~/.codex/sessions/`. A passing
run costs about $0.03 of Claude usage. The Codex Subagent used about 31,000
input tokens, of which 24,000 were cached, and 33 output tokens.

## What it sets up and sends

- **Config Document:** `provider.claude.permissionMode` is pinned to
  `bypassPermissions`, so nothing in the tree asks. `derivation.errand` is
  `off`, so no Title or Icon Errand spends Model calls.
- **Execution Directory:** a scratch Git repository with one commit.
- **Session:** a Claude Session on `haiku`, or on Claude's default Model where
  `haiku` is not offered.
- **Prompt:** it names the Broker but none of its Tools.

> Using Suru's Broker, spawn one Subagent on the codex Provider, on that
> Provider's default Model, named echo. Tell it to run the shell command
> `sleep 8` and then reply with exactly the word PONG and nothing else. Do not
> do its task yourself, and do not wait for it or check on it: once it is
> spawned, end your turn straight away. Suru will bring you its Report as a new
> message; when it arrives, answer with the single word DONE followed by the
> Subagent's reply.

The `sleep 8` makes the Subagent outlast the Agent's first Turn. Its Report then
reaches an idle Agent and begins a Continuation, rather than steering the Turn
that spawned it. Each live stretch is bounded at 300 s. A wait fails at once, printing
the Session's Turns, Messages and Activities, when a Turn fails, an Approval or
Questionnaire is raised, or the Agent's first Turn settles with no spawn.

## What it asserts

1. **The Claude Session:** it starts at `bypassPermissions`.
2. **The row:** the Turn the Prompt began holds exactly one Subagent row. The
   row is `brokered`, is named `echo`, and leads to the child Session.
3. **The tree:** the Subagent tree stream's snapshot lists the child under the
   parent. The Session listing does not list it.
4. **The child Session:** its parent is the Claude Session and it runs on
   Provider `codex`, in the parent's Execution Directory. Its posture is
   `never` with `danger-full-access`, derived from `bypassPermissions`
   (ADR 0036). Its Transcript opens with a Delegation Message from the parent's
   Agent that carries the task.
5. **The child's Turn:** it settles `Completed`, the Agent recorded on it runs
   on `codex`, and its final Agent Message contains `PONG`.
6. **The row settles:** it settles `Completed`, gives a duration, and stays in
   the Turn that spawned the Subagent.
7. **The Continuation:** the parent then holds exactly two Turns. The Prompt's
   Turn is `Completed`, and it settled no later than the second Turn began. The
   second Turn is a Continuation: no Prompt began it and it is `Completed`. It
   holds only Agent Messages, since the Report stands nowhere, and they contain
   `DONE` and `PONG`. The parent's Transcript holds no Command row, so neither
   the Broker calls nor the `ToolSearch` that loads them project one. Nothing
   in the tree is Working any more.
8. **The settled tree:** reopened, it shows the child `Completed`, on the Model
   the child's Provider confirmed.
9. **Shutdown:** the Server shuts down within 60 s.

## The live runs

All four runs used the same build and CLIs. The runs were launched from inside
a Claude Code session, so every inherited `CLAUDE*` variable was removed first;
a user's own terminal carries none. Runs 1–3 went through transparent capture
wrappers, named by `SURU_CLAUDE_PATH` and `SURU_CODEX_PATH`. Each wrapper saved
every launch's arguments, stdin and stdout. The Claude wrapper added
`--debug-file`, and the Codex wrapper set `RUST_LOG` for Codex's stderr. Run 4
used the installed binaries directly.

| Run | Outcome |
| --- | --- |
| 1 | Failed at the spawn deadline, 304.9 s. Claude listed `suru` as `connected` but registered none of its Tools. The Agent searched with `ToolSearch` five times and called `mcp__suru__list_providers` once, getting "No such tool available". It gave up after 26 s. See the finding below; fixed in `f4be4bc`. |
| 2 | Failed at 4.4 s, before any Model call. The smoke's own check that the Agent had given up held before the first Turn was recorded. Fixed in the smoke. |
| 3 | Passed in 41.3 s, with capture. |
| 4 | Passed in 34.8 s, without capture. |

Run 4's milestones:

| Elapsed | Event |
| --- | --- |
| 17.3 s | The Claude Agent spawned the Subagent. The time includes both catalog discoveries and the Claude launch. |
| 32.6 s | The Codex Subagent's Turn settled, with `PONG`. |
| 34.3 s | The Report woke the Claude Agent, which answered `DONE PONG`. |
| 34.8 s | The Server shut down. |

### Finding: the Broker's Tool list was invalid under the 2026-07-28 revision

Claude 2.1.283 opens every MCP connection with a `server/discover` probe. The
#408 capture's endpoint was hand-written and answered it with `-32601`, so
Claude fell back to `initialize` at 2025-11-25. The Broker's own transport,
rmcp 3.4.1, implements discovery instead. It answers with supported versions
2024-11-05 through 2026-07-28. Claude then speaks 2026-07-28, which its debug
log calls the "modern" era:

```
MCP server "suru": Connection established with capabilities: {"hasTools":true, …,
  "protocolEra":"modern","negotiatedProtocolVersion":"2026-07-28"}
```

That revision has no `initialize`. Each request carries its revision and the
client's capabilities in `_meta`, with `Mcp-Method` and, for a call, `Mcp-Name`
headers. It also requires every list result to carry `ttlMs` and `cacheScope`.
rmcp leaves both fields unset unless the server sets them, and the Broker did not
set them. Claude rejected all four `tools/list` answers (the first try and three
retries, 250 ms to 1 s apart), then gave up:

```
[ERROR] "MCP server \"suru\" Failed to fetch tools: Invalid result for tools/list:
  [{"expected":"number","path":["ttlMs"], …},
   {"values":["public","private"],"path":["cacheScope"], …}]"
```

`f4be4bc` sets both fields on the Broker's `tools/list`: `ttlMs: 0` and
`cacheScope: "private"`. The list is stale at once and private to the token that
asked, the same as rmcp's own answer to `server/discover`. Clients on earlier
revisions ignore the extra fields: the Codex Subagent, at 2025-06-18, listed
the Tools and reported the server `ready`. A protocol test now runs the
2026-07-28 lifecycle against the Broker:
`a_harness_speaking_the_2026_07_28_revision_is_answered_a_tool_list_it_can_register`.

### The Claude side (run 3)

- **Launch:** one CLI process served the whole Session, with `--mcp-config
  /tmp/suru-claude-broker-<id>.json --allowedTools mcp__suru__*
  --append-system-prompt <note> --permission-mode bypassPermissions`.
- **`system` `init`:** it listed `{"name": "suru", "status": "connected",
  "source": "dynamic"}` beside `{"name": "claude.ai Claude Docs", "status":
  "connected", "source": "claudeai"}`. It also listed the four
  `mcp__suru__*` Tools and `ToolSearch`.
- **Tool calls:**
  1. `ToolSearch` with `select:mcp__suru__list_providers,mcp__suru__spawn_subagent`.
  2. `mcp__suru__list_providers`, which completed in 10 ms.
  3. `mcp__suru__spawn_subagent` with
     `{"provider": "codex", "model": "gpt-6-astra", "name": "echo", "description": "Run sleep 8 then reply PONG", "prompt": "Run the shell command `sleep 8` and then reply with exactly the word PONG and nothing else."}`.
     It answered the child's `session_id` in 433 ms.
- **First Turn:** the Agent wrote "Subagent spawned … Standing by for its
  report." and ended with `result` (`num_turns: 4`).
- **The Report:** it reached Claude on stdin as one user message:

  ```
  Subagent Report from Suru: the Subagent "echo" you delegated to through the Broker
  completed after 19.9s. Its session_id is <id>, which read_subagent takes.

  Its final Message:

  PONG
  ```

  Claude answered on the same process with a fresh `system` `init`, the text
  `DONE PONG`, and a second `result` (`num_turns: 1`). Suru recorded that as
  the Continuation.
- **Approvals:** no `can_use_tool` request came at any point.
- **Shutdown:** Claude logged `MCP server "suru": HTTP connection closed after
  34s (cleanly)`.

### The Codex side (run 3)

- **`thread/start`:** it carried the following, and Codex answered it without
  error:

  ```json
  {"cwd": "<scratch repository>", "approvalPolicy": "never", "sandbox": "danger-full-access",
   "ephemeral": false, "developerInstructions": "Suru, the app hosting this session, …",
   "config": {"mcp_servers.suru": {"url": "http://127.0.0.1:<port>/broker",
     "http_headers": {"Authorization": "Bearer <token>"},
     "tool_timeout_sec": 900.0, "default_tools_approval_mode": "approve"}}}
  ```

- **MCP startup:** `mcpServer/startupStatus/updated` reported `suru` `starting`,
  then `ready`.
- **Delegation:** `turn/start` carried `model: gpt-6-astra` and `effort: medium`
  with the Delegation:

  ```
  Delegated to you through Suru by the Agent working on "Using Suru's Broker, spawn one
  Subagent on the codex Provider, on that Provider…".

  Run the shell command `sleep 8` and then reply with exactly the word PONG and nothing else.
  ```

  With Title derivation off, the parent's Title is its Prompt, cut short.
- **Work:** the Subagent ran `/usr/bin/zsh -lc 'sleep 8'` without asking
  (exit 0, 7.9 s), wrote `PONG`, and its turn completed in 18.2 s. No approval,
  elicitation or user-input request reached Suru.
- **Developer instructions:** the thread's rollout
  (`~/.codex/sessions/…/rollout-…-<thread>.jsonl`) holds a `developer` message
  carrying the Broker's note.
- **HTTP:** Codex's HTTP client made exactly three POSTs to the Broker:
  `initialize` (200, protocol 2025-06-18), `notifications/initialized` (202),
  and `tools/list` (200). It made no GET and no DELETE.

## Live facts from the earlier slices

| Fact | Result |
| --- | --- |
| `server/discover` before `initialize` gets `-32601` and the client falls back (#410) | **Contradicted for the Broker.** rmcp answers the probe and Claude speaks 2026-07-28, which caused the finding above. The `-32601` came from #408's hand-written capture endpoint. |
| A GET on the endpoint (405) is tolerated (#410) | **Not exercised.** Neither client opened a GET stream. On a 2026-07-28 connection Claude opens none when the server advertises no `listChanged` ("no auto-opened subscriptions/listen on a modern connection"). Codex opened none against the stateless Broker. |
| A DELETE at shutdown is tolerated (#410) | **Not exercised.** Neither client sent one. The stateless Broker issues no `mcp-session-id`, so a client has no session to end. |
| Claude reads `--mcp-config` from a file path (#411) | **Confirmed.** Its debug log shows the `suru` HTTP transport with `timeoutMs: 900000`, read from the file. |
| The user's own MCP servers still load beside `suru` (#411) | **Confirmed.** The claude.ai connector `claude.ai Claude Docs` connected beside `suru`. This machine has no user-scoped servers in `~/.claude.json`, so that connector is the only user server on record. |
| Claude honours `--append-system-prompt` in stream-json print mode (#414) | **Confirmed.** The Prompt names no Tool, yet the Agent's first call selected `mcp__suru__list_providers` and `mcp__suru__spawn_subagent` by name, which only the note gives. |
| The Agent finds the Broker's Tools through `ToolSearch` (#414) | **Confirmed once the Tool list is valid.** Run 1's searches found none. |
| `--allowedTools mcp__suru__*` keeps Broker calls from asking (#411) | **Consistent, not isolated.** No `can_use_tool` came, but `bypassPermissions` would allow the calls anyway. |
| Codex accepts `tool_timeout_sec: 900.0` as a float (#411) | **Confirmed.** `thread/start` was answered and the server reported `ready`. |
| Codex honours `developer_instructions` on `thread/start` (#414) | **Confirmed** by the rollout's `developer` message. The same on `thread/resume` is **not exercised**. |
| A Report to an idle Claude Agent, sent as a stdin user message, starts a Turn with an extra `result` (#418) | **Confirmed.** A new `init`, the answer, and a second `result` came on the same process, and Suru recorded a `Completed` Continuation. |
| A Codex child under a Claude `default` parent runs `untrusted` and may raise Approvals (#417) | **Not exercised.** The smoke pins `bypassPermissions`, and the child ran `never` with `danger-full-access`, unasked. |
| Windows: the owner-only ACL on the MCP config file (#411) | Out of reach on this Linux machine. |

## Decision

The smoke proves the Claude→Codex path end to end against the real CLIs. A
Claude Agent finds the Broker's Tools from the appended note and calls them
without an Approval. Suru spawns a brokered Codex Subagent in its own Session,
in the parent's Execution Directory, at the posture derived for it, and opens
its Transcript with the Delegation. The Subagent's row settles with its Turn.
The Report wakes the settled Claude Agent into a Continuation on the same
process, where the Agent acts on it. The run also found a real defect, now
fixed: Claude registered no Broker Tools at all, because the Broker answered a
revision it had offered without the fields that revision requires.

Follow-ups, none blocking:

- **Long calls under 2026-07-28.** #408's request, idle and call timers, and
  progress resetting the 300 s idle window, were measured at 2025-11-25.
  Claude now speaks 2026-07-28 to the Broker. Before `wait_subagents` (#419)
  relies on those timers, repeat that capture against the Broker's revision.
  The alternative is to stop advertising 2026-07-28.
- **Copilot as parent or child.** Not hosted by the smoke. Whether Copilot
  merges `mcp_servers` with the user's own servers (#411) is still open.
- **Windows.** The file ACL, and the smoke itself, which is `cfg(unix)`.
- **Steer races.** A Report reaching a Claude Agent whose Turn still works is
  avoided by design, since the `sleep 8` keeps the Subagent working past the
  Agent's first Turn.
- **Resume after restart.** A brokered Subagent resumed through its Resume
  State, and `developer_instructions` on `thread/resume`.
- **Asking postures.** A Claude `default` parent with an `untrusted` Codex
  Subagent answering its Approvals, which would also isolate the `--allowedTools`
  allowlist.
- **Send and wait (#419).** Outside this slice.
