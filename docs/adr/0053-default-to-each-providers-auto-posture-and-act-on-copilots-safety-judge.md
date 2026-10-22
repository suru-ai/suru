# Default to each Provider's auto posture, and act on Copilot's safety judge

Copilot gains an `auto` Approval Posture beside `ask` and `allowAll`. Under it, Suru asks Copilot to run in its assisted permission mode. In that mode, Copilot's safety judge reviews each permission request and attaches a recommendation. Suru approves the requests the judge recommends approving and surfaces every other request as an Approval. Requests the judge doubts, excludes or fails to review all ask the user. So does any request reviewed by no judge, such as one that arrives before Copilot has applied the mode. A managed policy that requires an explicit human decision still outranks the judge. Every posture other than `auto` puts Copilot back in its manual mode, so the judge is never consulted where its answer would be ignored.

This refines ADR 0026's rule that Suru never answers a request on the user's behalf. When the user picks `auto`, they choose to delegate to the judge, just as Claude's `auto` delegates to Claude's classifier. Suru still decides nothing on its own judgement. Each approval Suru gives on the judge's word carries the judge's provenance back to Copilot.

`auto` is the built-in default for both Providers that offer it: Claude and Copilot. Suru's Codex posture has no such value, so Codex keeps `on-request`.

## Considered Options

- **Build Suru's own judge.** Rejected because Copilot already runs one and reports its reasoning per request. A second judge would cost a model call per request and could disagree with the first.
- **Let Copilot approve on its own in assisted mode.** Not available: the mode is advisory, and the host owns every decision.
- **Keep `ask` as the default.** Rejected because the user wants routine work to run unasked by default, and both judges still ask where they are unsure.

## Consequences

- Copilot's assisted mode is marked experimental in its SDK. If Copilot refuses the mode, or applies another one, Suru logs the refusal and keeps `auto`. With no recommendations arriving, every request asks, which errs in the safe direction.
- Copilot's `auto` sits at level 2 of ADR 0036's table, and becomes Copilot's value at levels 2 and 3.
