# Copilot Subagent steer (#397)

Captured 2026-09-27 against GitHub Copilot CLI **1.0.87** (`/usr/bin/copilot`,
signed in), the pinned `github-copilot-sdk` **1.0.12-preview.0**, and the
official SDK checkout at `1644e74578db3637bc7527951bac227aabbc0584`. The main
agent ran `gpt-5-mini`; the CLI chose `gpt-5.6-luna` for its `task` agents
(`general-purpose` agents inherited `gpt-5-mini`).

## Finding: a `write_agent` to a working Subagent never steers it

Seven live runs, every one with the recipient still working when the message
was sent. In every run the message was **queued**, never steered. It reached the
recipient only after that agent's stretch had ended.

| Run | Transport | Sender | Recipient working on | Delivery |
| --- | --- | --- | --- | --- |
| 1 | `copilot -p --output-format json` | main agent | one combined `bash` call | `queued` |
| 2 | `copilot -p --output-format json` | main agent | first of three separate `bash` calls | `queued` |
| 3 | `copilot -p --output-format json` | sibling (`scope: "siblings"`) | first of three separate `bash` calls | `queued` (the sender also queued a copy to itself) |
| 4 | `copilot -p --output-format json` | sibling (`scope: "siblings"`) | a **sync** `task` agent | skipped: "write_agent only supports background agents" |
| 5 | SDK stream (`examples/copilot_steer_capture.rs rpc`) | host, `session.tasks.sendMessage` | first `bash` call | `queued` (RPC answered `sent: true`) |
| 6 | SDK stream (`… parent`) | main agent | first of three separate `bash` calls | `queued` |
| 7 | SDK stream (`… sibling`) | sibling, `agent_id` found with `list_agents` | first of three separate `bash` calls | `queued` |

In runs 2, 3, 6 and 7 the recipient completed two or three more tool rounds
after the send. None of those rounds took the message in. The SDK documents
`steering` for `session.send({ mode: "immediate" })` on the main loop, falling
back to `queued` when a steer arrives too late
(`docs/features/steering-and-queueing.md`). No agent-to-agent route reached
that mode. `TasksSendMessageResult.sent` is documented as "delivered or
steered", but run 5 steered nothing either.

So **no fixture records a `steering` delivery to a Subagent**. The steer path
Suru implements follows the SDK's schema for `user.message` alone and remains
unverified live.

## What fired, and when

Runs 6 and 7 are committed, sanitized, as
`tests/copilot_integration/fixtures/steer-parent-queued.jsonl` and
`steer-sibling-queued.jsonl`. Their order of events:

1. The sender's `tool.execution_start` for `write_agent` fires, and its
   `tool.execution_complete` follows within about 1 ms. The result is
   `success: true`, with content `Message delivered to agent <id>. Use
   read_agent to check the agent's response.`, or `… The recipient can reply
   with write_agent.` when a sibling sent it. Nothing fires on the recipient's
   side at send time: no `user.message` and no `pending_messages.modified`
   carrying its `agentId`.
2. The recipient keeps working its stretch without the message:
   `assistant.turn_start` / `assistant.turn_end` pairs with `turnId` `"0"` …
   `"3"`, all under the spawn's `interactionId`. Its final reply ignores the
   message.
3. The stretch ends with `subagent.completed`, named by the spawning tool call.
4. Then, and only then, about 15–25 ms later, the recipient gets `user.message`
   carrying its `agentId`. So the event fires when the message is **consumed**,
   not when it is sent. Its fields are `delivery: "queued"` and `turnId: "0"`:
   the first agent-loop turn of a **new run**, since turn numbering restarts.
   It also carries a fresh `interactionId`, a `messageId`, a `parentAgentTaskId`,
   and `transformedContent` with a `<current_datetime>` prefix.
5. The new run follows: `assistant.turn_start` (`turnId: "0"`, the new
   `interactionId`), a reply that does act on the message (it says PINEAPPLE),
   then `assistant.turn_end`. No second `subagent.started` opens it, and no
   `subagent.completed` closes it, although `subagent.started` said
   `resumable: false`. Afterwards the main loop gets `system.notification`
   with `kind: { type: "agent_idle", agentId, agentType, description,
   displayName }`.

The spawn's own prompt also arrives on the recipient as a `user.message`, with
`delivery: "idle"`, just before its first `assistant.turn_start`.

### `source`

- The main agent sends as `agent-<Copilot Session id>`. In run 6 that was
  `agent-94bf4cb4-…`, which is the Session's own id; no Subagent has that
  instance id. The spawn's `idle` prompt and the host's
  `tasks.sendMessage` (with no `fromAgentId`) read the same way.
- A sibling sends as `agent-<sibling's agentId>`, the same instance identity its
  events carry in the envelope's `agentId`. In run 7 the sibling had already
  settled (`subagent.completed`) 45 s before the recipient consumed its message.

### `write_agent` arguments

- `agent_id` (string): the recipient's instance identity, as the `task` result
  and `list_agents` report it.
- `message` (string).
- `scope` (`"siblings"` | `"children"`), in place of `agent_id` to address a
  group. The Model also described an `agent_ids` array, but no run used it.

The CLI does not log the tool's schema. These names come from the calls the
Models made (runs 1–7) and the Model's own summary of its tool. The SDK's
`nodejs/src/toolSet.ts` lists `write_agent`, `read_agent`, `list_agents` and
`send_inbox` as built-ins.

### Late arrival

Every send in these runs was late in the sense that matters: queued, then run
after `subagent.completed`. A send timed against a tool-free final reply (the
Claude r3 shape) would add nothing, because a send landing during tool rounds
already waited for the stretch to end. `delivery: "queued"` never came with a
second `subagent.started`.

## Adapter behavior

- A `user.message` carrying a working Subagent's `agentId` with
  `delivery: "steering"` becomes `ProviderEvent::SubagentSteered`, at the point
  it arrives, and it begins no Turn. Its sender comes from `source`:
  `agent-<id>` naming any Subagent instance opened on the timeline (settled or
  not) is attributed to that Subagent, and any other `source` to the owning
  Session. **Unverified live** (see above).
- `idle` and `queued` deliveries and a missing delivery project nothing, as does
  a steer for a Subagent that is not working or unknown. A queued message
  arrives after `subagent.completed`, when its Subagent is no longer routed, so
  it lands nowhere. Reading it, and the unannounced run it starts, as a resume
  is #398.
- `write_agent` executions project nothing in the sender's Transcript, from the
  main agent or a Subagent, whether they succeed, fail, or stay open when the
  loop stops. Before this change each one showed as a `write_agent {…}` Command.
  Because of the queueing above, a `write_agent` currently leaves no trace in
  either Transcript until #398 lands.
- A `user.message` without an `agentId` (the user's own Prompt or steer) still
  projects nothing.

## Capture and sanitizing

`examples/copilot_steer_capture.rs` drives the installed CLI through the SDK the
way Suru's Provider does: streaming, sub-agent streaming events included, and
approve-all permissions. It writes every `session.event` as one JSON line. The
committed fixtures keep the stream order and the event envelopes. Sanitizing
dropped two groups of event types:

- Event types that carry environment, account, plugin, or MCP details:
  `session.start`, `session.managed_settings_resolved`, `session.mcp_*`,
  `session.tools_updated`, `system.message`, `hook.*`, `model.*` (request and
  assignment diagnostics), `prompt_cache_break`, `session.usage_checkpoint`,
  and `sandbox.decision`.
- Bulk progress Suru does not read: `assistant.streaming_delta`,
  `assistant.tool_call_delta`, and `session.background_tasks_changed`. The last
  would also make a replay ask the scripted CLI for `session.tasks.list` while
  its timeline is still playing.

It also stripped these payload keys: `apiCallId`, `providerCallId`,
`serviceRequestId`, `quotaSnapshots`, `encryptedContent`, `reasoningOpaque`,
`reasoningBlocks`, and `toolTelemetry`. The capture's working directory reads
`/tmp/workspace`. The replay tests also skip the `permission.*` entries. The
capture answered them with approve-all, so a replay would open Approvals whose
withdrawal races the timeline.
