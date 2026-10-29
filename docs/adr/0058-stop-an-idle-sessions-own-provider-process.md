# Stop an idle Session's own Provider process

A Provider actor outlived the Turns it ran, idle and connected, until its tree was evicted (ADR 0022) or the Server stopped. A tree is evicted only once nothing holds it, and a reader following it holds it, so a Session left open in a Client — or a long orchestration whose dozens of brokered Subagents all sat under one followed tree — kept a Provider process, its anchor shell and their pipes for every idle Session. A release Server reached macOS's default 256 open files with about seventy such Sessions, after which nothing it spawned could start (#587). We decided a Session whose Provider launches a process of its own for each Session has that process stopped once it has waited idle for `ServerTimings::provider_idle_shutdown` (ten minutes by default) with nothing owed on it; the actor stays, and the next Turn opens a fresh connection that resumes the conversation from its stored Resume State, as a restart's first Turn does.

A runtime says whether it launches a process per Session through `ProviderRuntime::launches_a_process_per_session`. Claude and Codex do: each Session runs its own `claude` or app-server. Copilot does not: every Session shares one process, which costs nothing more for each idle Session, so its connections are kept. The default is `false`, keeping every connection.

The idle wait restarts with every input the actor reads — a command or a Provider event — and when it elapses the process is stopped only where nothing would die with it or needs it running: no Turn active, no Prompt or Delegation waiting, no Report owed, no native Subagent routed or working, no Watch Outcome held, and nothing the store holds as work on the actor's Sessions. That last is eviction's own account of work running or owed — Watches standing, brokered Subagents still owing their delegator output, open Interventions, Reports owed to a Sidekick — taken over the Sessions riding the actor, and without the readers eviction also counts, since what a reader sees does not change when the process stops. Anything still owed keeps the process for another wait.

## Considered Options

- **Cap the live Provider runtimes and evict the least recently used.** Rejected: any cap is either too low for a long orchestration or too high to keep under the file limit, and stopping the least recent Session when a new one starts makes starting a Session pay for others' idleness.
- **Only warn as descriptor use approaches the limit.** Rejected: it tells the user of a problem they cannot act on short of closing Sessions, and the Server still fails at the limit.
- **Stop idle connections for every Provider.** Rejected: a shared process is not freed by one Session letting go of it, so Copilot would only pay a resume for nothing.

## Consequences

- An idle Session's next Turn starts its Provider process again, paying its start-up and a resume; that is the cost a restart already pays.
- A Report reaching an Agent whose process was stopped waits for its next Turn, as one reaching an evicted tree does; the process is never stopped while Reports are owed to it or brokered Subagents still work for it.
- Raising the Server's soft open file limit at start (also #587) lifts the ceiling; this keeps idle Sessions from reaching it.
