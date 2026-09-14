# Choose Icons by name from a Suru-owned Catalog

Suru replaces the experimental per-Session emoji with a Nerd Font Icon for each Session and each Workspace, derived by the same Title Errand or chosen by the user from a picker. A Model cannot reliably emit a Nerd Font glyph: the glyphs live in Unicode's Private Use Area, the Model has never seen them rendered, and it hallucinates rare names. We decided that every Icon, whether derived or chosen, comes from an Icon Catalog Suru itself ships — a hand-written table of roughly 150 to 250 glyph names with codepoints and search keywords, pinned by a test against Nerd Fonts' published `glyphnames.json` — and that an Icon is stored as its Catalog name, never its codepoint. The Errand's reply schema carries the Catalog as a strict enum, so a reply is valid by construction and works under Codex's strict-schema requirement, and the picker's grid is that same Catalog.

## Considered Options

- **Freeform Nerd Font names validated against the full ten-thousand-entry glyph table**: rejected because Models know the common names and invent the rest, so the range gained is mostly range that fails validation, and a picker cannot present ten thousand cells.
- **Storing codepoints**: rejected because it welds rows to one Nerd Fonts release; a name lets the Catalog remap or retire a glyph, with a retired name simply drawn as no Icon.
- **Keeping the Emoji alongside**: rejected because Suru already has one Setting governing every Nerd Font glyph it shows, and one mechanism should mark work, not two.

## Consequences

The Catalog is a curated, reviewable artifact, and growing it is an edit to that table plus a check that the glyph renders in the fonts people install. Derivation only ever fills an absence, so a user's choice always stands and no origin flag is needed. Workspaces gain their first owned property and, with it, a table of their own.
