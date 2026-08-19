# Durable session storage in SQLite

Sessions, their Transcripts, Agent Selections, and provider Resume State are persisted in a single user-scoped SQLite database owned by the server, so Sessions survive server replacement. This supersedes the "without prematurely committing to durable history" clause of ADR-0003; the rest of that decision (server-authoritative Sessions, thin-client boundary) stands. In-memory state remains canonical while the server runs: storage is a write-through mirror fed by a background writer that coalesces streaming content updates, so a hard crash may lose the tail of an in-flight Turn but never a completed one.

## Considered Options

- **Schema shape**: an event log of `SessionChange` replayed through `apply_update` (t3-code-style event sourcing) was rejected because it welds the storage format to the churning wire protocol, forcing a data migration per protocol change. Instead, entities are rows with typed identity/ordering/metadata columns and a JSON payload column for content (opencode-style). A whole-snapshot blob per session was rejected because writes would scale with transcript length.
- **Payload compatibility**: pre-1.0, stored payloads are decoded tolerantly and best-effort; a session whose payloads no longer decode is surfaced as unreadable, not migrated and not a startup failure. Schema migrations, by contrast, always run: forward-only, each in its own transaction, auto-applied at startup, refusing to open a database newer than the binary.
- **Access layer**: Diesel, chosen for compile-time-checked queries (type safety was the deciding criterion) and because its embedded SQL migrations auto-applied via `diesel_migrations` are the de-facto standard we wanted. SeaORM (Rust-code migrations, but runtime-built queries) and sqlx (compile-checked raw SQL, no entity layer) were rejected. Diesel is synchronous, which matches SQLite; database access is confined behind the storage seam on blocking tasks.

## Consequences

- The database lives in the XDG data dir (`~/.local/share/chidori/chidori.db` in release); development builds insert a Channel path segment. The state dir adopts the same rule (release omits the segment) so one path convention covers both roots.
- Config never lives in the database: the database is machine-owned state, config is human-edited input designed separately.
- Provider credentials never live in the database; provider auth remains delegated to provider binaries.
- Resume State is stored per (Session, Provider) as an opaque JSON payload so future providers need no schema change.
