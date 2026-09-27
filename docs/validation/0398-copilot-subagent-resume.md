# Copilot Subagent resume (#398)

Captured 2026-09-27 against GitHub Copilot CLI **1.0.88** (`/usr/bin/copilot`,
signed in), the pinned `github-copilot-sdk` **1.0.12-preview.0**, and the
official SDK checkout at `1644e74578db3637bc7527951bac227aabbc0584`. The main
agent ran `gpt-5-mini`; the CLI chose `gpt-5.6-luna` for its `task` agent. The
#397 captures (CLI 1.0.87) supply the `queued` half of what this doc records.

## Finding: a message to a settled Subagent runs it again, unannounced

One live run through the SDK stream (`examples/copilot_steer_capture.rs idle`).
The main agent started a background `task` agent that ran one `bash` call and
reported, waited for it with `read_agent (wait: true)`, and only then sent it a
`write_agent`. The message reached the agent as `user.message` with
`delivery: "idle"`, and the same agent ran again on its own context: its report
named the word from each stretch.

Together with #397's `queued` runs this fixes the shape of every late delivery
Copilot makes to a Subagent:

| Sent while the recipient was | `delivery` | Delivered at | New `subagent.started`? | Closing event? |
| --- | --- | --- | --- | --- |
| working (#397, runs 1–3, 5–7) | `queued` | 15–25 ms after `subagent.completed` | no | none |
| settled (this run) | `idle` | ~20 ms after the sender's `write_agent` completed | no | none |

Neither shape is a steer, and both are what CONTEXT.md's Delegation entry calls
a resume: a Delegation that begins a Turn, delivered once the earlier work had
finished. ADR 0033 records the decision.

## What fired, and when

The sanitized capture is `tests/copilot_integration/fixtures/resume-idle.jsonl`.

1. The spawn: `tool.execution_start` for `task`, `subagent.started` (with
   `resumable: false`, a wire field neither pinned SDK types), the agent's
   `user.message` with `delivery: "idle"` carrying the spawn prompt, one `bash`
   round, a closing `assistant.message` with `toolRequests: []` and `phase:
   "final_answer"`, `assistant.turn_end`, then `subagent.completed`.
2. The main agent's `read_agent` completes, it reasons, and its `write_agent`
   `tool.execution_start` / `tool.execution_complete` fire 1 ms apart with
   `success: true`.
3. About 20 ms later the agent gets `user.message` with `delivery: "idle"`, the
   same `agentId`, a fresh `interactionId`, `turnId: "0"`, and
   `source: agent-<Copilot Session id>`, the main agent's identity as in #397.
4. The new run: `assistant.turn_start` (`turnId: "0"`), a `bash` round, then
   `assistant.turn_start` (`turnId: "1"`), a closing `assistant.message` with
   `toolRequests: []`, and `assistant.turn_end`. No `subagent.started` opened it
   and no `subagent.completed` closed it.
5. No `system.notification` of kind `agent_idle` fired at all in this run. In
   #397 it named the resumed agent in the parent run and never fired for the
   resumed agent in the sibling run. It is not a settle signal.
6. The main agent's second `read_agent` completes with the new report, it
   answers, and `assistant.idle` then `session.idle` end the loop.

### The loop's exit is the only close

In every resumed run captured (this one and #397's two), the run ended at the
`assistant.turn_end` following a model message with no `toolRequests`, which is
how Copilot's agent loop exits. A model message that requests tools is followed
by another `assistant.turn_start` of the same `interactionId`. `phase:
"final_answer"` marked those closing messages too, but the field is documented
as belonging to "phased-output models" and the `gpt-5-mini` main agent never
carried it, so it is not relied on.

`session.idle` is deferred while any agent runs (`AssistantIdleData` docs), so
it bounds every resumed run from above.

## Adapter behavior (decided, not yet implemented)

- A `user.message` carrying the `agentId` of a Subagent whose stretch has
  settled is a resume, whatever its `delivery`: `ProviderEvent::SubagentResumed`
  attributed to the Agent `source` names, with the message as the Delegation
  that opens the new Turn. The spawn's own `idle` prompt precedes any work by
  the Subagent and projects nothing, as today.
- The resumed Turn settles completed at the Subagent's `assistant.turn_end` that
  follows a model message with no tool requests; a later `user.message` for the
  same Subagent, or the loop's `session.idle`, settles one still open; the
  aborted idle of an interrupt settles it interrupted.
- Every `user.message` a settled Subagent consumes is its own resume. Copilot
  fires the event on consumption, so a second one cannot arrive before the
  previous run ended.
- `agent_idle` notifications project nothing.

Implementation is tracked in the follow-up issue linked from #398.

## Capture and sanitizing

`examples/copilot_steer_capture.rs idle` is the mode that recorded this. The
sanitizing recipe is #397's: the same event types dropped, the same payload keys
stripped, and the working directory rewritten to `/tmp/workspace`. The replay
tests should also skip `permission.*` entries, as #397's do.
