# Resume a Subagent in its own Session, and record each resume as its own row

Claude (through `SendMessage`) and Codex (through `send_input` / `followup_task`, after `resume_agent` for a closed child) can hand more work to a Subagent whose work has Settled: the same agent, continuing the same conversation. We represent that as a **resume**: a Delegation that begins a new Turn in the Subagent's existing Session, so that Session holds the agent's whole conversation, while the parent's Transcript gains a new Subagent row in the Turn that delegated the resume, leading into that same Session. Several rows can therefore share one child Session, but the tree and the Subagents Section still show one entry per Subagent, placed where it first spawned. A Subagent itself never Settles; only its Turns and rows do. So that a resume can find its Session after Suru restarts and relaunches the Provider with `--resume`, the Provider's own identity for the Subagent (Claude's task id, Codex's child thread id) is stored with the child Session. A resume Suru cannot place is recorded as a new Subagent rather than dropped.

## Considered Options

- **A new child Session per resume.** This was the stopgap in 5c3c92e. It split one conversation across N Sessions, each missing the context the agent itself had, and listed 17 entries for two agents.
- **Putting the original row back to Working.** Rejected: it rewrites history in a Turn that already Settled and may be folded away. The Transcript is ordered history, and the resume happened later, in another Turn.
- **Keeping the Subagent identity in memory only.** Rejected: it breaks "one Subagent, one Session" whenever a Provider conversation outlives the Suru process.

## Consequences

- Anything that reads Subagent rows must de-duplicate by child Session: the Subagent tree, the Subagents Section, and the Subagent Picker.
- A Subagent entry's Marker comes from its Session's latest Turn. Its time is summed over all its Turns. Neither is read from any one row.
- Each row's duration and outcome describe only its own stretch.
- Stopping and Interventions follow the Subagent's current Turn, whichever row began it.
- Copilot doesn't take part: its Subagents report themselves as not resumable.
