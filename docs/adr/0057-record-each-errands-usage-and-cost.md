# Record each Errand's Usage and Cost

ADR 0011 had Suru keep nothing of an Errand but its answer. Errands cost real money, though, and Usage Metrics and Insights could not count what they never saw, so their Cost and Tokens under-stated what Suru used. We decided every Errand that reaches a Provider is recorded with the Usage it consumed and its Cost. This amends ADR 0011: an Errand still belongs to no Session, appears in no Transcript, and is never a Turn, but Suru now keeps a record of it beside its answer.

A Provider's Errand call answers with its Usage, and its Cost where it knows one, alongside the reply. A Provider that cannot tell answers with neither. Claude reads `usage` and `total_cost_usd` from the print-mode result it already parses; print mode persists no conversation, so that running total is the Errand's own, and it is a Reported Cost. Codex and Copilot answer with none for now.

The Errand runner records the Errand once it has reached its Provider, however it ends: what it was for (Session Title, Title proposal, or Workspace Description), the Provider and Model it ran at, when it began and ended by the Server's clock, its outcome (answered, failed, timed out, or abandoned), and its Usage, Cost, and Cost Basis, each absent where unknown. An Errand that timed out or was abandoned — by a shutting-down server, or by a caller that stopped waiting — records its Usage as unknown, since Suru stopped listening before the Provider could say. A failed Errand records it unknown too, because a Provider's failure carries no Usage. An Errand that never reached a Provider, because its Provider is not hosted or is turned off, records nothing.

Records go to an `errands` table in SQLite, written by the storage writer on its own thread and indexed by when each Errand began. The runner hands a record over without waiting for it to land, so recording never delays an answer, and a record that cannot be handed over or written is logged and never fails one. The records are never loaded at startup: a reader asks storage for the Errands that began in the interval it counts, on a blocking task, as the Activity tallies are read (ADR 0056).

## Considered Options

- **Attribute an Errand's Usage to the Session it served.** Rejected: an Errand belongs to no Session, and a Workspace Description serves no single Session at all. Folding Suru's own work into a Session's Cost would make that Cost say more than the user's work.
- **Store Errand records as rows of a Session's history.** Rejected: that is the ephemeral-Session machinery ADR 0011 turned down, and it would put Errands in Transcripts.
- **Hold Errand records in memory, as Turn metadata is.** Rejected: nothing but Usage Metrics and Insights reads them, and only on request; a query by interval over an indexed table costs less than keeping every record resident for the life of the Server.
- **Skip timed-out and abandoned Errands.** Rejected: they reached a Provider and may have cost money, so leaving them out would quietly under-state Cost. Recording them with Usage unknown lets a total say it is partial.

## Consequences

- Every Errand that reaches a Provider adds one row; nothing prunes them, as nothing prunes Turns.
- A failed Errand's Usage is unknown even where the Provider spent tokens before failing, until a Provider's failure can carry what it consumed.
- Codex and Copilot Errands carry no Usage until their runners learn to read it, so figures including them read as partial.
- Recording is best-effort: a write that fails costs that Errand's figures, never its answer.
