# Bound Cost Coverage by the Provider actor, and show own Cost beside tree Cost

A Provider's whole-tree amount covers only the descendants that Provider ran itself: the native Subagents reached from its Session without crossing a brokered Session. A brokered Subagent heads a coverage domain of its own. Its own reporting lifetimes cover its own native descendants, and its Costs are added beneath any ancestor's whole-tree amount, never suppressed by it. This amends ADR 0025, whose rule that a whole-tree amount "includes descendants" was written when every descendant was native. ADR 0035 then gave a brokered Subagent a Provider actor of its own, whose work its caller's Provider never meters: Claude's `total_cost_usd` counts the Subagents its own process spawned, and a Codex Subagent under a Claude caller could never be inside a Claude meter at all. Both the live roll-up and the startup fold read the boundary from the stored `brokered` flag, so no stored Turn changes (ADR 0016) and existing histories read correctly on their next start.

Every Session also carries its **own Cost** beside its tree Cost. The own Cost is the same Cost Coverage applied to that Session's Turns alone: each reporting lifetime's latest cumulative report counts once, and Turn-scoped Costs add. For Codex and Copilot that is exactly the sum of the Session's own Turns. For Claude it is the cumulative figure Claude reports for the conversation, which includes the native Subagents Claude ran inside that process, because Claude publishes no split. The footer reads `$<own> ($<tree>)` where descendants moved the figure and `$<tree>` otherwise, and it drops the own figure first when it runs out of room.

## Considered Options

- **Deciding coverage from Session ancestry and time overlap alone.** Rejected: that is the rule this replaces. Every brokered child works while its caller's process is alive, so it was always suppressed as if its caller had paid for it.
- **Estimating a Claude Session's own work apart from its native Subagents from its own tokens.** Rejected: Claude publishes the tokens but not the split, and an Estimated own figure beside a Reported tree figure would not add up.
- **Summing a Session's own Turns naively for its own Cost.** Rejected: a Claude lifetime's cumulative reports would count several times over.

## Consequences

- A whole-tree amount still covers its native descendants' unknown Costs, overlapping amounts still contribute once, and a partial total stays partial.
- The own Cost is derived and carried beside the tree Cost on the snapshot, the summary, and the change that moves them, so a listing and an open Session state the same two figures.
- A Subagent's view brackets its own subtree and never its caller's spend.
