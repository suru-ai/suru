# Deliver Subagent Reports as Provider input, and give a brokered Subagent its own Provider actor

When a brokered Subagent's Turn settles, Suru delivers a Subagent Report — its outcome and a bounded excerpt of its final Message — to the delegating Agent as that Agent's Provider's own input: a stream-json user message for Claude, `turn/start` for Codex (which steers when a turn is running), and `session.send` in immediate mode for Copilot. An idle Agent wakes into a Continuation; a working Turn is steered. This is the pattern Claude and Copilot use for their own background subagents, whose completion notifications start a new turn in an idle parent; Codex never starts a parent turn itself, offering only a blocking `wait_agent`, so Suru supplies the wake-up for it. A bounded `wait_subagents` Tool stands beside the Report for an Agent that needs a result before it can go on. A brokered Subagent is a full Session with its own Provider actor, because its Provider differs from the root's: today one actor per root Session routes interrupt, stop, Approvals, posture delivery, and Continuation-owing through native Subagent routes only, and each of those paths becomes tree-generic so a brokered child is stopped, answered, and continued like a native one.

## Considered Options

- **Blocking only.** Rejected: a spawn that blocks for an hour is hostage to three harnesses' tool timeouts, and the parent can do nothing meanwhile.
- **Push only.** Rejected: Codex's models are trained on `wait_agent` and would poll a status Tool in a loop.
- **Running the child through the root's actor.** Impossible across Providers, and it is why the root-only routing has to open up.
- **Attributing a spawn by correlating it with the projection's tool_use stream.** Rejected for the reason ADR 0032 refused it for Claude steers: it matches on timing and arguments.

## Consequences

- A Report stands nowhere in any Transcript; the settled Subagent row is the record, and the child's own Transcript holds its words. The row is added by the Broker at spawn, in the caller's active Turn, and each Provider's projection absorbs the Broker's tool calls rather than showing them.
- A Report whose Agent has no Provider process waits for the head of that Session's next Turn; Suru never relaunches a Provider to deliver one. A Report delivered to a Session the user had settled makes it active again.
- A brokered child the user stops on its own is reported as stopped; an interrupt of the parent stops its children and reports nothing. A brokered child keeps working when its parent's Provider process ends.
- A Report to a settled native Subagent wakes a Continuation of that Subagent's own Session, as a Watch would.
- A spawn made by a native Claude or Copilot Subagent is attributed to the Session its token names, and when that Session has no active Turn a Continuation is begun to hold the row, until a capture shows a per-subagent signal in those harnesses' calls. Codex's `_meta.threadId` attributes its spawns exactly.
- The wait Tool defaults to 60 seconds, bounded to 10 and 600, with progress every 30 seconds, and returns `timed_out` so the Agent may call again.
- Broker Tools never raise an Approval: Claude allowlists `mcp__suru__*`, Codex's per-thread config sets the server's default approval mode to approve, and Suru's own Copilot handler approves the server.
