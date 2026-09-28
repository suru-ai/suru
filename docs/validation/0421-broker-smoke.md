# The Broker smoke with the installed CLIs (#421)

Last passed 2026-09-28 against Claude Code **2.1.283** (`/usr/bin/claude`, a
wrapper that sets `DISABLE_UPDATES=1` and runs `/opt/claude-code/bin/claude`,
signed in with a claude.ai account) and Codex CLI **0.157.1** (`/usr/bin/codex`,
signed in with a ChatGPT account). The delegating Agent ran Claude's `haiku`
(`claude-haiku-4-5-20251001`). The Subagent ran the Model the Agent chose from
`list_providers`: Codex's catalog default, `gpt-6-astra`, at that Model's default
effort, `medium`.

`tests/broker_smoke.rs` hosts a real Server with the real `ClaudeRuntime` and
`CodexRuntime` and drives it only through the client protocol. In both of its
smokes, a Claude Session's Agent spawns a Codex Subagent through the Broker.

- **Report smoke:** the Subagent's Turn settles, and its Subagent Report wakes
  the Claude Agent into a Continuation, where it answers.
- **Wait smoke:** the Claude Agent holds a `wait_subagents` call open for the
  335 s the Subagent works, and answers from what the wait returns.

## Running it

```
SURU_BROKER_SMOKE=1 cargo nextest run --test broker_smoke --run-ignored ignored-only --no-capture
```

`cargo test --test broker_smoke -- --ignored --nocapture` runs them too. The
binary holds two tests, each gated twice:

- **The Report smoke,** `a_claude_agent_spawns_a_codex_subagent_through_the_broker_and_answers_its_report`:
  the Claude Agent spawns a Codex Subagent and ends its Turn, and the
  Subagent's Report wakes it into a Continuation.
- **The wait smoke,** `a_claude_agents_wait_on_a_codex_subagent_outlasts_claudes_idle_window`:
  the Claude Agent holds a `wait_subagents` call open on a Codex Subagent that
  sleeps 330 s, longer than Claude's 300 s idle window for a silent HTTP MCP
  call.

Add `-E 'test(/answers_its_report/)'` or `-E 'test(/wait_on_a_codex/)'` to run
one of them.

- **Ignore attribute:** both carry `#[ignore = "set SURU_BROKER_SMOKE=1 to use the
  installed, signed-in Claude and Codex binaries"]`. `cargo nextest run` skips
  them (`0 tests run: 0 passed, 2 skipped` for this binary). Run alone,
  `cargo nextest run --test broker_smoke` then prints `error: no tests to run`
  and exits 4, unless `--no-tests=pass` is passed; the full suite's exit is
  unaffected. `cargo nextest list --message-format json` reports both
  `ignored: true`, and they are listed only with `--run-ignored ignored-only`
  or `all`.
- **Runtime check:** without `SURU_BROKER_SMOKE=1`, an ignored-only run prints
  `skipping: set SURU_BROKER_SMOKE=1 to opt in` and returns.

Once opted in, each smoke refreshes the Model Catalog and skips, printing the
reason, when Suru finds either CLI not installed or not signed in. It fails if a
Provider's catalog fails or reports an incompatible version.

`SURU_CLAUDE_PATH` and `SURU_CODEX_PATH` name other binaries, as they do for
Suru itself. `SURU_LOG` (for example `warn,suru=info,rmcp=debug`) writes the
Server's tracing to stderr. Each smoke prints both CLIs' `--version` first,
then the elapsed time at each milestone.

Using the real CLIs leaves their own records behind: Claude's under
`~/.claude/projects/` and Codex's rollouts under `~/.codex/sessions/`. A passing
Report smoke costs about $0.03 of Claude usage. Its Codex Subagent used about
31,000 input tokens, of which 24,000 were cached, and 33 output tokens.

## What they set up and send

- **Config Document:** `provider.claude.permissionMode` is pinned to
  `bypassPermissions`, so nothing in the tree asks. `derivation.errand` is
  `off`, so no Title or Icon Errand spends Model calls.
- **Execution Directory:** a scratch Git repository with one commit.
- **Session:** a Claude Session on `haiku`, or on Claude's default Model where
  `haiku` is not offered.
- **Prompts:** they name the Broker but none of its Tools. The Report smoke
  names none at all; the wait smoke names `wait_subagents`.

The Report smoke's Prompt:

> Using Suru's Broker, spawn one Subagent on the codex Provider, on that
> Provider's default Model, named echo. Tell it to run the shell command
> `sleep 8` and then reply with exactly the word PONG and nothing else. Do not
> do its task yourself, and do not wait for it or check on it: once it is
> spawned, end your turn straight away. Suru will bring you its Report as a new
> message; when it arrives, answer with the single word DONE followed by the
> Subagent's reply.

The `sleep 8` makes the Subagent outlast the Agent's first Turn. Its Report then
reaches an idle Agent and begins a Continuation, rather than steering the Turn
that spawned it.

The wait smoke's Prompt:

> Using Suru's Broker, spawn one Subagent on the codex Provider, on that
> Provider's default Model, named echo. Tell it to run the shell command
> `sleep 330`, which takes over five minutes, to wait for that command to
> finish, and only then to reply with exactly the word PONG and nothing else.
> Do not do its task yourself. Once it is spawned, call wait_subagents on it
> with timeout_seconds 400 and wait for that call to answer, calling nothing
> else meanwhile. When it answers with the Subagent settled, reply with the
> single word DONE followed by the Subagent's reply, and end your turn. If the
> call fails or times out instead, reply with WAIT FAILED followed by the error
> it gave, and end your turn without calling it again.

The `WAIT FAILED` branch tells a call Claude cut short apart from one that
answered, and records Claude's error at the moment it happened.

Each live stretch of the Report smoke is bounded at 300 s, and the whole wait
smoke at 600 s. A wait fails early, printing the Session's Turns, Messages and
Activities:

- **At once** when a Turn fails, an Approval or Questionnaire is raised, or the
  Agent's first Turn settles with no spawn. In the wait smoke, also when the
  Agent says `WAIT FAILED`.
- **After 30 s** when the Session has stood at rest, unchanged: nothing Working
  and every Turn settled, and the state waited for not reached. A Session
  passes through rest for a moment on its way to a Continuation, between the
  commit that settles its Subagent's row and the one that opens the Turn the
  Report wakes. So rest counts only once it has lasted.

## What the Report smoke asserts

1. **The Claude Session:** it starts at `bypassPermissions`.
2. **The row:** the Turn the Prompt began holds exactly one Subagent row. The
   row is `brokered`, carries `echo` in its name (compared case-insensitively),
   and leads to the child Session.
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

## What the wait smoke asserts

1. **Spawn and child:** as items 1–4 of the Report smoke.
2. **The wait answered:** the Agent's Messages in the Turn that waited come to
   contain `DONE` and `PONG`, and never `WAIT FAILED`. That Turn is never
   failed or interrupted.
3. **Only once the Subagent had slept:** the answer arrives at least 330 s
   after the spawn, and the child's row has settled `Completed`.
4. **The child slept it out:** the child's Turn settled `Completed` with
   `PONG`, after working at least 330 s. So the call that answered was that
   long.
5. **The settled tree and shutdown:** as items 8–9 of the Report smoke.

It waits for the answer rather than for the Turn that waited to settle. The
Subagent's Report steers that Turn as the wait answers, and the Turn does not
settle then, because of the second finding below.

## The live runs

Every run used Claude Code 2.1.283 and Codex CLI 0.157.1. Runs 1–4 ran the
Report smoke on this branch before it was rebased. Runs 5 and 6 ran on the
branch rebased onto #419's `send_to_subagent` and `wait_subagents` (`main` at
`5a475ad`). The runs were launched from inside a Claude Code session, so every
inherited `CLAUDE*` variable was removed first; a user's own terminal carries
none. Runs 1–3 and 5 went through transparent capture wrappers, named by
`SURU_CLAUDE_PATH` and `SURU_CODEX_PATH`. Each wrapper saved every launch's
arguments, stdin and stdout. The Claude wrapper added `--debug-file`, and the
Codex wrapper set `RUST_LOG` for Codex's stderr. Runs 4 and 6 used the
installed binaries directly.

| Run | Smoke | Outcome |
| --- | --- | --- |
| 1 | Report | Failed at the spawn deadline, 304.9 s. Claude listed `suru` as `connected` but registered none of its Tools. The Agent searched with `ToolSearch` five times and called `mcp__suru__list_providers` once, getting "No such tool available". It gave up after 26 s. See the first finding below; fixed in the branch's `fix(broker)` commit. |
| 2 | Report | Failed at 4.4 s, before any Model call. The smoke's own check that the Agent had given up held before the first Turn was recorded. Fixed in the smoke. |
| 3 | Report | Passed in 41.3 s, with capture. |
| 4 | Report | Passed in 34.8 s, without capture. |
| 5 | Wait | The wait call survived, but the smoke failed at its 600 s deadline, with capture. The Agent answered `DONE PONG` 335 s into its `wait_subagents` call, and the Subagent's row settled `Completed`. Then the Turn that waited never settled. See the second finding below. |
| 6 | Both | Both passed without capture: the Report smoke in 46.0 s, the wait smoke in 359.4 s. The wait smoke now asserts the answer rather than the Turn's settling. |

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

The branch's `fix(broker)` commit sets both fields on the Broker's `tools/list`: `ttlMs: 0` and
`cacheScope: "private"`. The list is stale at once and private to the token that
asked, the same as rmcp's own answer to `server/discover`. Clients on earlier
revisions ignore the extra fields: the Codex Subagent, at 2025-06-18, listed
the Tools and reported the server `ready`. A protocol test now runs the
2026-07-28 lifecycle against the Broker:
`a_harness_speaking_the_2026_07_28_revision_is_answered_a_tool_list_it_can_register`.

### The wait smoke: a 335 s Broker call at 2026-07-28 (runs 5 and 6)

Claude negotiated 2026-07-28 with the Broker in run 5, as in run 3. The Agent
loaded `mcp__suru__spawn_subagent` and `mcp__suru__wait_subagents` with
`ToolSearch`, then called `list_providers` and spawned the Subagent. It then
called `wait_subagents` with
`{"ids": ["<child>"], "timeout_seconds": 400}` at 03:48:26:

- **Debug log:** Claude logged `Tool 'wait_subagents' still running (Ns elapsed)`
  every 30 s from 30 s to 330 s, then `completed successfully in 5m 35s`
  (`tool_dispatch_end … outcome=ok durationMs=335410`). Nothing aborted at
  60 s, at 300 s, or at the Subagent's 330 s.
- **Stdout:** Claude emitted a `tool_progress` heartbeat with
  `elapsed_time_seconds` every 30 s, eleven in all.
- **The answer:**
  `{"settled":[{"duration_ms":337645,"message":"PONG","session_id":"<child>","status":"completed"}],"timed_out":false,"timeout_seconds":400}`.
  The Agent replied `DONE PONG`.
- **The Subagent:** it ran `/usr/bin/zsh -lc 'sleep 330'` (exit 0, 329.9 s),
  polling the command six times with a short message each time, then wrote
  `PONG`. It used about 208,000 input tokens, of which 197,000 were cached,
  and 521 output tokens. The Claude side cost $0.044.

Run 6 repeated it without capture: the answer came 341.1 s after the spawn,
and the Subagent worked 338.4 s.

This settles the question #408 left open. At the 2026-07-28 revision, under the
Broker's configuration, a Broker call stays open past Claude's 60 s request
timer and 300 s idle window. The per-server `timeout` is 900 s, and the
Broker sends progress every 30 s while it waits. The run does not isolate
which of the two keeps the idle window open. The `tool_progress` lines are
marked `heartbeat: true`, and neither Claude's debug log nor Suru's trace shows
whether an MCP progress notification arrived. At 2025-11-25, #408's run 5 showed
the per-server `timeout` alone raising the idle window.

### Finding: a Report steering a Claude Turn in a Broker call leaves the Turn open

In run 5 the Subagent's Report was written to Claude's stdin as a steer of the
Turn that waited. It was written in the same millisecond that the
`wait_subagents` answer arrived, since the settling that answers a wait also
builds the Report. Claude's own transcript
(`~/.claude/projects/<cwd>/<session>.jsonl`) records what it did:

```
03:54:01.899 queue-operation enqueue
03:54:01.901 user: tool_result {"settled": … "PONG" …}
03:54:01.899 attachment queued_command {"prompt": [{"type": "text", "text": "Subagent Report from Suru: …"}]}
03:54:01.905 queue-operation remove
03:54:04.070 assistant: DONE PONG
```

The CLI folded the queued Report into the loop's next request as a
`queued_command` attachment, and answered both with one `result`
(`num_turns: 6`). Its stdout carries nothing to say a queued message was taken
in: no user echo, and only `status: requesting` between the tool result and
the next message. Suru's Claude steer accounting
(`src/provider/claude/turn_in_flight.rs`, from #129) assumes "the CLI answers
every user message queued into a running loop with a `result` of its own". It
counted two `result`s owed and received one, so the Turn stayed `Active`. The
Session read Working until the smoke's deadline and the Server's shutdown.

**Scope:** every Claude `wait_subagents` that answers because a Subagent
settled hits this, since its Report is always queued at that same moment. By
the same mechanism, a user's steer Prompt that reaches a Claude Turn mid-tool
call probably does too; that was not run live. A Report to an idle Claude Agent
(the Report smoke) is unaffected. This is larger than a smoke fix, so it is
left as the first follow-up below.

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
| Claude's 60 s request timer and 300 s idle window do not cut a long Broker call short (#408, #419) | **Confirmed at 2026-07-28** for a 335 s `wait_subagents` (runs 5 and 6). The per-server `timeout` and the Broker's progress are not told apart. |
| The CLI answers every message queued into a running loop with a `result` of its own (#129, relied on by #418's steers) | **Contradicted** for a message queued while a tool call is running. The CLI folds it into the loop's next request as a `queued_command` and answers both with one `result` (run 5). |
| A Codex child under a Claude `default` parent runs `untrusted` and may raise Approvals (#417) | **Not exercised.** The smoke pins `bypassPermissions`, and the child ran `never` with `danger-full-access`, unasked. |
| Windows: the owner-only ACL on the MCP config file (#411) | Out of reach on this Linux machine. |

## Decision

The Report smoke proves the Claude→Codex path end to end against the real
CLIs. A Claude Agent finds the Broker's Tools from the appended note and calls
them without an Approval. Suru spawns a brokered Codex Subagent in its own
Session, in the parent's Execution Directory, at the posture derived for it,
and opens its Transcript with the Delegation. The Subagent's row settles with
its Turn. The Report wakes the settled Claude Agent into a Continuation on the
same process, where the Agent acts on it.

The wait smoke proves that a Broker call held open 335 s survives Claude's
timers and answers the Agent in the Turn that waited.

**The Broker speaks MCP 2026-07-28 to Claude 2.1.283, and stays there.** That is
the newest revision rmcp 3.4.1 offers in its `server/discover` answer, and
Claude negotiates the newest. The Broker does not pin a lower revision:

- **It is compliant there.** Its `tools/list` now carries the `ttlMs` and
  `cacheScope` that 2026-07-28 requires. Without them Claude registered none of
  the Broker's Tools (run 1).
- **The timers do not bite.** The long-call concern, #408's timers measured at
  2025-11-25, does not reproduce at 2026-07-28: a 335 s `wait_subagents`
  answered normally (runs 5 and 6).
- **Codex is unaffected.** It still initializes at 2025-06-18 and passes over
  the two extra fields.

The fallback, if a later Claude cuts long calls at 2026-07-28, is to override
`supported_protocol_versions` on `BrokerServer` in `src/broker/mcp.rs` with
`Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))`,
keeping the `ttlMs`/`cacheScope` fields.

Follow-ups:

- **Claude steer accounting (a real defect, not a smoke gap).** A Report
  steering a Claude Turn still in a Broker call leaves that Turn `Active` and
  the Session Working indefinitely. This happens after every Claude
  `wait_subagents` that answers because its Subagent settled. See the second
  finding. Possible directions:
  - Learn when the CLI folds a queued message into the running loop, for
    example through `--replay-user-messages`, which needs a capture first.
    This would also fix a user's steer that lands mid-tool call.
  - Stop counting a `result` per steer, and let a second loop become a native
    Continuation.
  - Do not deliver a Report for a Subagent a wait has just answered with.
    That reverses #419's recorded choice that a Report still arrives.
- **Which mechanism keeps a long call alive at 2026-07-28.** A capture that
  records the Broker's progress notifications would tell the per-server
  `timeout` from progress apart.
- **Copilot as parent or child.** Not hosted by the smokes. Whether Copilot
  merges `mcp_servers` with the user's own servers (#411) is still open.
- **Windows.** The file ACL, and the smokes themselves, which are `cfg(unix)`.
- **Resume after restart.** A brokered Subagent resumed through its Resume
  State, and `developer_instructions` on `thread/resume`.
- **`send_to_subagent` live.** Neither smoke sends to a Subagent.
- **Asking postures.** A Claude `default` parent with an `untrusted` Codex
  Subagent answering its Approvals, which would also isolate the `--allowedTools`
  allowlist.
