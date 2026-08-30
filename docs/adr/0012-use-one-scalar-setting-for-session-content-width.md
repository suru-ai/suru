# Use one scalar Setting for Session content width

The Client Setting `session.contentWidth` governs the width of the Session Content Column as one value: `"fill"` uses the normally padded available width, while an integer of at least 50 caps the column at that many terminal columns. Its built-in default is `80`; a capped column is centered and shrinks rather than clips when the terminal is narrower. The Session header keeps the normally padded terminal width, and the Landing is unaffected.

## Considered Options

- **Separate mode and maximum Settings**: rejected because they admit contradictory or latent state, such as `fill` paired with a maximum whose future effect is unclear.
- **A tagged object containing mode and width**: rejected because the object repeats what the value's shape already says. The scalar forms `"fill"` and `80` are unambiguous and keep one intent in one Setting.
- **Cap only the Transcript**: rejected because the Transcript and composer are one working surface. The cap instead covers the Session Content Column: Transcript and Working Indicator, queued Prompts, latest-position affordance, composer extensions, composer, composer footer, and composer autocomplete.

## Consequences

Integer widths are open Setting values rather than a finite list: the settings panel shows them as `max 80 columns` and opens a numeric editor to choose one, while Space selects the named `fill` value. Selecting a maximum from `fill` begins at `80`; `fill` retains no hidden width. Invalid pins are ignored with the usual per-key diagnostic, and an invalid editor value remains open with the minimum explained.

A changed value reflows an open Session immediately while preserving its Transcript anchor as closely as rewrapping permits. The effective column width is a transcript projection input, and pointer interaction is bounded horizontally so the empty gutters of a centered column do nothing.
