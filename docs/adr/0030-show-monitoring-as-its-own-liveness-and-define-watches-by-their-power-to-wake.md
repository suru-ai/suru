# Show Monitoring as its own liveness, and define Watches by their power to wake the Agent

A Provider can settle its Turn while work the Agent started keeps running and may wake it later. Claude backgrounds a shell or a Monitor and ends its loop with `result`, and the task's settling queues a notification that begins a Continuation. Suru read such a Session as idle, because Working is derived only from owed Prompts, unsettled Turns, and surviving Subagents (ADRs 0015 and 0024). We now name that work a **Watch** and give a Session whose only live work is Watches a liveness of its own, **Monitoring**. Monitoring is a Standing between Failed and Done, has its own elapsed time, rolls up through the Session tree the way Working does, and yields to Working anywhere in the tree. What makes something a Watch is that its settling or reporting **may wake the Agent**, not that it is a process still running. A Subagent is never a Watch: an Agent thinking and spending tokens is Working, even when the Turn that spawned it has settled.

## Considered Options

- **Fold the waiting into Working, with a third Working Indicator state beside "Waiting for subagents".** Rejected: the Sidebar would then say Working for a Session doing nothing but tailing a log for an hour, which is the confusion that prompted this change.
- **t3code's model:** "monitoring" only when watch loops are the sole live work, with a Subagent's own shells ignored because the Subagent covers them. We keep its Working/Monitoring split but not the ignoring. A *background* Subagent's shells and Monitors outlive the Subagent (shells for up to an hour, Monitors indefinitely) and wake that Subagent when they fire, so ignoring them reads a live tree as idle. A Watch belongs to the Session whose Agent started it, including a settled Subagent's Session.
- **Count every process the Agent left running.** Rejected: Monitoring means the Session is still waiting for something, and nothing waits on a process that cannot wake the Agent.

## Consequences

- **Codex Sessions never read Monitoring.** Codex's background terminals outlive a Turn, but nothing wakes the Agent when they exit, so they are not Watches.
- **Only a Watch's settling is recorded,** as a Watch Outcome Activity in the Turn it wakes the Agent into. Claude sends Suru nothing for a Monitor's individual reports, so a Continuation one of them begins stays unexplained rather than guessed at.
- **A background Command's Activity still settles with its tool call.** It is not held open for the life of its Watch: the tool call really has returned, and holding the row open would break ADR 0015's rule that a settled Turn accepts no Provider output.
- **Monitoring is never stored.** Every Watch dies with its Provider process, so no Session is Monitoring when Suru starts, and Watches lost that way record no outcome.
- **Interrupting stops Watches.** Interrupting a Monitoring Session stops the Watches in its subtree and settles nothing, because no Turn is open.
- **Monitoring counts as activity for housekeeping.** A Monitoring Session's Managed Worktree is never Reclaimed, and the Session never auto-settles.
- **A Watch that wakes a settled Subagent resumes it** in a Continuation of the Subagent's own Session, sharing the reopening mechanism of #378.
- **Copilot's attached background shells stay out of scope.** They keep Copilot's Turn open until `session.idle`, so they read as Working until #379 moves Copilot's settle point to the end of the loop.
