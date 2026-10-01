# Vary the Broker's Tools by caller, and know a Sidekick by its Workspace

The Tools that work across Suru itself — listing and reading Sessions, prompting and interrupting them, changing Settings, keeping Memories — are served from the Broker's one endpoint, which answers a Sidekick's Session with more Tools than it answers any other. A Session is a Sidekick's because its Workspace is the Sidekick Workspace, a directory Suru owns beside its data, and for no other reason: the Server derives it from what a Session already stores, so no flag is stamped at creation and nothing about the request that begins a Session changes. ADR 0034's per-Session token already tells the Broker who is calling; this decision makes the answer depend on it.

A brokered or native Subagent of a Sidekick is offered the ordinary Tools only, so work a Sidekick delegates cannot reach across Suru on its own.

## Considered Options

- **A second endpoint for the Sidekick's Tools.** Rejected: each of the three Providers attaches the Broker through its own seam (a config file, a per-thread entry, a per-session server list) and lifts its Approvals by server name, and a second server would repeat all of it to carry a distinction the token already makes.
- **A typed flag set by `/sidekick`.** Rejected: it needs a new field on Session creation and storage to say what the Execution Directory already says, and it would make a Session begun in that directory any other way silently not a Sidekick.
- **Offer the Tools to any Session behind a Setting.** Rejected for now: every Agent working in a Repository would be able to prompt and interrupt the user's other work. Widening who is offered a Tool is a later change to the same caller-dependent answer.

## Consequences

- What a Provider lists for the `suru` server is no longer one constant, so the instructions Suru appends at Session start are per caller too.
- The directory is the identity: reaching the Sidekick Workspace through the Sidebar's path entry makes a Sidekick as surely as `/sidekick` does.
