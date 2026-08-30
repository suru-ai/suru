# Per-run Log files in the state dir

Suru processes write structured Logs through the `tracing` ecosystem into `<state>/suru/log/`, one file per process run (named `<timestamp>-<role>-<pid>.log`), pruned to the newest few on startup. Both roles log: the server because it is long-lived and shared, the TUI client because it holds raw mode and the alternate screen, so a Log file is its only diagnostic outlet. Filtering is dev-facing only for now — the `SURU_LOG` env var with `EnvFilter` directives, defaulting to `warn,suru=info` — with real settings deferred.

## Considered Options

- **Location**: a literal mirror of opencode (`<data>/log/`, beside the database) was rejected because the XDG spec names logs as state-dir content, Suru's operational files (`runtime.json`, `server.lock`, `server.log`) already live in the state dir, and ADR-0006 frames the data dir as durable user session data. We adopt opencode's `log/` subdirectory shape, in the state root; the Channel path segment applies as it does to every state path.
- **File granularity**: opencode v2's single shared append-only file (processes disambiguated by a run-id field, no rotation) was rejected because unbounded growth undermines "attach your Log to a bug report", and their own history argues against it — v1 kept timestamped per-run files pruned to the newest ten, and their desktop app still rotates. Per-run files also give each concurrent TUI its own file, so the filename carries run identity and no cross-process append interleaving exists.

## Consequences

- `server.log` — the launcher's redirect of the detached server's stdout/stderr — is deliberately kept as a dumb crash net, separate from structured Logs: it catches exactly what no in-process subscriber can (panics, pre-init failures), an integration test greps it, and startup errors embed its tail.
- Payload policy: provider request/response bodies (user prompts, code) may appear only at `debug`/`trace`; auth material (the descriptor token, provider credentials) never appears at any level.
- The retained-file count and default filter are hardcoded constants, not config machinery.
