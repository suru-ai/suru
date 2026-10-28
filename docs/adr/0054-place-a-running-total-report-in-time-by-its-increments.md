# Place a running-total report in time by its increments

A Session-subtree report is a running total within its reporting lifetime: Claude's `total_cost_usd` restates everything the conversation has cost so far on every result. Cost Coverage (ADR 0025, ADR 0040) counted only the latest report in each lifetime, which is right for a tree Cost but says nothing about when the money went, and a reading that divides Cost by time, Model, or Session role needs to know which Turn each part belongs to. We decided Cost Coverage reports a **contribution** for each Turn: what that Turn adds to a total once every overlap is removed, with the Turn and Session it belongs to, its Cost Basis, and whether it is partial. A Turn-scoped Cost contributes its whole amount. A running-total report contributes its **increment**: what it adds to the highest total reported before it in the same lifetime, the first report contributing all of it. A report lower than one before it contradicts the running total, so it contributes nothing and is partial. Coverage removes overlaps exactly as before: a native descendant an ancestor's whole-tree amount covers contributes nothing, a brokered Subagent keeps its own contributions, prior records from earlier lifetimes count as their own series, and a Sidekick's Subsessions count once beneath it.

The increments of one lifetime add up to the highest total it reported, so the contributions under a root add up to that root's tree Cost and those of a Session's own Turns to its own Cost. Both Costs are now read as those sums, and the startup fold reads running totals through the same increments, so there is one reading of Cost Coverage rather than one for totals and another for placing Cost.

## Considered Options

- **Attribute each lifetime's latest report to the Turn carrying it.** Rejected: a long conversation's whole Cost would land on its last Turn, and on the day that Turn ran.
- **Increment over the previous report rather than the highest.** Rejected: after a report that fell, the next one would count again what the lifetime had already reported, so the increments would add up to a figure no report stated.
- **Spread a running total over its Turns by their tokens.** Rejected: that invents a split the Provider did not supply, which ADR 0025 and ADR 0040 refuse; Claude's increment goes to the Turn carrying the report, native Subagents included.
- **A second Cost Coverage reading for metrics.** Rejected: two readings drift, and a figure divided by time would stop adding up to the tree Cost a listing shows.

## Consequences

- A lifetime whose reports never fell reads exactly as before. One whose report fell now reads its highest total rather than its latest, marked partial; Claude's projection already refuses a report that falls within a process, so this is the reading of a history that contradicts itself rather than of ordinary work.
- A Turn can carry more than one contribution: a running total and a prior record from another lifetime, or a partial mark beside an amount. Work whose Cost is unknown, or that runs past the report covering it, is stated as a contribution with no amount that leaves the total partial, so a total over any slice of Turns is partial exactly where its work is not wholly priced.
- No stored Turn changes (ADR 0016): increments are derived from the frozen Costs at every reading, and reading them moves no Session revision.
