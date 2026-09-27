# Serve the Broker over loopback HTTP with a per-Session token

The Broker — the Tools Suru itself offers every Agent, through which one Provider's Agent spawns a Subagent on another — is served as a streamable-HTTP MCP endpoint on the Server's existing loopback axum listener, and the calling Session is identified by a bearer token minted when that Session's Provider is opened and retired when it closes. Each harness receives the endpoint and its token in the one per-Session seam it has: Claude's `--mcp-config` on its per-Session process, Codex's `config` map on `thread/start` and `thread/resume` (`mcp_servers.suru` with `url` and static `http_headers`), and Copilot's `mcp_servers` on create and resume, which is the only per-Session seam Copilot offers because one CLI process serves every Copilot Session. The alternative — a `suru mcp --session <id>` stdio proxy each harness spawns — was rejected because it costs a process per Provider Session, needs tokio's `io-std`, and would only relay to the same HTTP API; all three harnesses accept static headers, so nothing was gained by it.

## Considered Options

- **A stdio proxy subcommand.** Rejected as above. Its one advantage, Claude's 30-minute idle window for stdio servers against 5 minutes for HTTP, is answered by progress notifications on long calls.
- **Reusing the Server's single bearer token.** Rejected: the Broker must know which Session is calling, and a token any child process can read from `runtime.json` grants the whole API rather than one Session's Agent.

## Consequences

- Claude's HTTP MCP path times each request out at 60 seconds and aborts a silent call after five minutes, so the per-server `timeout` is raised and the Broker sends progress notifications during a wait. Codex's `tool_timeout_sec` is set through the same per-thread config map, and Copilot's per-server `timeout` likewise.
- Codex's own suite never sets `mcp_servers` through `thread/start`; the path is the generic override mechanism and needs a validation capture before it is relied on.
- The token is re-minted at every Provider relaunch — `--resume`, `thread/resume`, `resume_session` — and lives in the Session's Provider configuration, never in `runtime.json`, whose reader rejects unknown fields.
- A native Claude or Copilot Subagent runs inside its parent's process and so calls the Broker with its parent's token (ADR 0035 says how such a spawn is attributed). Codex carries the calling thread id in each call's `_meta`, so its calls are attributed exactly.
