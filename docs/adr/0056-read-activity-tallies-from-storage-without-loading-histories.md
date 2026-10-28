# Read Activity tallies from storage without loading histories

Insights counts how the Agents worked: which Tools they used, and how often those uses failed. Those uses are Activities, and ADR-0022 keeps a Session's Activities in SQLite until that Session is opened; only its Turns are in memory from startup. Loading every history the range reaches to count its Activities would cost the first-open latency and memory ADR-0022 avoids, many times over. And hydrating a Session is not a pure read: it settles stale Turns and withdraws Prompts.

We decided the Insights read asks storage for Activity tallies directly and loads no history. Under the store lock it copies Turns and how each Session stands in its forest, as the metrics read does. Outside the lock it works out which Turns of which counted Sessions the interval holds. Then, on a blocking task, it queries the `activities` table for those Sessions, filtered by Session id in batches of a few hundred so no query binds more parameters than SQLite allows. Each row gives up its owning Turn and, through `json_extract` on its payload, only its kind, status, Tool name, and MCP server. No payload is decoded whole and nothing is installed in the store. The rows stream into a tally in Rust, keeping only those whose Turn the interval holds. An Activity's time is its owning Turn's, taken from the in-memory Turn metadata.

The read goes to storage for every Session, including one already open in memory, so every Session is counted the same way. ADR-0022 has storage follow the in-memory history at idle moments and at every Turn boundary. So for a Session that is working now, storage may lag by Activities not yet saved: a Tool used moments ago may not be counted until the next save lands. Insights accepts that brief lag. It reports a range of past work, and a refresh after the save counts the use.

## Considered Options

- **Hydrate the Sessions the range reaches.** Rejected: it loads whole Transcripts to count a few fields, holds them in memory until eviction, and has hydration's side effects, so a read would change what it reads.
- **Count from memory where a Session is open, and from storage otherwise.** Rejected: two sources for one figure, each with its own rules about what has been seen. The lag it would avoid lasts only until the next save.
- **Keep running Tool counts on each Turn, or in a table of their own.** Rejected for now: it adds a second consistency contract that would need a migration and a backfill, as ADR-0022 declined for its summaries. Revisit only if the read proves slow, and then with an ADR of its own.
- **Filter by Turn id in SQL.** Rejected: `activities` is indexed by Session, not Turn. All time would bind every Turn id. Filtering by Session uses the index, and the Turn check in Rust is cheap.

## Consequences

- The Insights read loads no Session, moves no revision, and changes nothing. The no-hydration tests pin this.
- Reading the Tools costs time in proportion to the Activity rows of the Sessions with Turns in the range. Those rows' payloads are read from disk but never decoded whole.
- A Session working now may count a recent Tool use only after its next save.
- A failed storage read fails the Insights read with a server error rather than reporting no Tools.
- Compactions and Approvals, also Activities, can be counted the same way: one more read of a few fields, beside this one.
