# Run Suru's own Provider work as Errands

Work Suru does for itself rather than for the user — deriving a Session's Title and Emoji today, compaction and summarization later — runs as an Errand: one Prompt to a Provider, one reply shaped by a schema the Errand supplies, carrying no Tools and belonging to no Session. A Provider fulfils an Errand through its harness's own one-shot mode where it has one (`codex exec --ephemeral`, `claude -p`), and otherwise by starting a Provider-side session and discarding it. The alternative — running Errands through the ordinary Session machinery behind an "ephemeral" flag — was rejected because it turns "this leaves no trace" into a rule five modules have to keep rather than a property of the seam, and because it hands a call that writes six words the whole apparatus of Tools and Resume State it must never use.

## Consequences

The escape hatch is deliberate and narrow. A Provider with no one-shot mode still runs Errands, so which Providers can do this work never depends on a harness feature Suru does not control; the cost is that such a session may leave a trace in that CLI's own state directory, which Suru neither polices nor pretends to. What the fallback never creates is a *Suru* Session: no `SessionStore` entry, no `sessions` row, no catalog change, no Transcript, and no stored Resume State, so nothing an Errand touches can be listed, opened, or resumed.

Because no Provider can be relied on to enforce the schema — a fallback session has no way to be handed one at all — the schema is a request rather than a guarantee, and whatever asks for an Errand validates the reply itself and holds something to fall back on. A Title that cannot be derived stays the one the Session's first Prompt gave it.
