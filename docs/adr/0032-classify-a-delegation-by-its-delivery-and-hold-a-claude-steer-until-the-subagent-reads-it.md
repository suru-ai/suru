# Classify a Delegation by its delivery, and hold a Claude steer until the Subagent reads it

A Delegation sent to a Subagent that is still working doesn't necessarily steer it. Claude queues a `SendMessage` to a running agent "for delivery at its next tool round". If the agent finishes without one, Claude restarts it with the message as a fresh prompt. That restart is a `task_started` with the same task id, whose `tool_use_id` is the original Agent call rather than the `SendMessage`. Copilot documents the same fallback: a `steering` message that arrives too late is `queued` and processed as its own run. So Suru classifies a Delegation by how the Provider delivered it. A Delegation that begins a Turn is a spawn or a resume. One delivered into a working Turn is a steer, and it stands in the Subagent's Transcript at the point the Subagent received it.

Claude's stream never reports that point. The Subagent's side of the stream carries no message for a steer, and the only signal is the parent's `SendMessage` result saying the message was queued. Suru therefore holds each queued steer as pending against its Subagent. It places pending steers, in the order they were sent, just before the Subagent's next assistant message, which is where Claude delivers them. A restart consumes the pending steer as the Delegation that opens its new Turn. A steer still pending when its Subagent is stopped was never delivered, and it stands nowhere. Codex needs no pending state: it emits a `UserMessage` item on the child thread under the active turn when it drains the input, so any `UserMessage` after a child Turn's first one is a steer.

## Considered Options

- **Classify by what the Subagent was doing when the Delegation was sent.** Rejected: Claude's late-steer restart would then be a Turn with no Delegation to open it, and it would contradict the Provider's own boundary (ADR 0015).
- **Place a Claude steer when `SendMessage` reports it queued.** Rejected: the steer would stand before output the Subagent produced without having read it.
- **Read the `queued_command` attachment from Claude's on-disk subagent JSONL.** Rejected: it gives the exact point, but only by polling a private file format.

## Consequences

- A resume begun by a late steer can start after the parent Turn that sent it has Settled. Its row then lands in whatever parent Turn is active, or begins a Continuation. This amends ADR 0031's "in the Turn that delegated it".
- A steer never changes the parent's Transcript. Codex's `sendInput` no longer revises the live Subagent row's description, and Copilot's `write_agent` call is absorbed rather than shown as a Command.
- Codex's V2 `send_message` / `followup_task` reach clients only as experimental raw response items. Until Codex surfaces them as thread items, a V2 steer is not shown.
