# Use a user-scoped reusable local server

_The loopback-only clause is superseded by ADR-0017, which adds an opt-in second listener for Server-to-Server Pairings; everything here still governs the local listener and its Clients._

Suru uses one long-lived local server per OS user and installation channel rather than one server per project or TUI. Clients discover the server through a locked, atomically published runtime descriptor, authenticate over loopback HTTP, reuse a matching build, and replace a mismatched build; the server survives after all clients disconnect so multiple projects and interfaces can share its orchestration state. Session updates use per-Session snapshot-first SSE streams with monotonic revisions, keeping clients thin while allowing them to recover from missed connections without an initial replay log. The separate server-lifecycle stream carries only connection keepalives and shutdown intent.
