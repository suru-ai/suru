# Adopt OpenCode's v1 theme document format for Themes

Suru's Themes, built-in and user-supplied alike, are JSON documents in the format OpenCode's TUI uses for its version-1 themes: a `defs` table of named colors and a `theme` table of semantic keys, where each value is a hex color, a reference into `defs`, an ANSI palette index, the word `transparent`, or a `{dark, light}` pair of any of those. Suru reads the keys its own role table consumes and ignores the rest. This lets Suru ship OpenCode's theme set by copying the files and lets users bring any theme written for OpenCode, at the cost of a vocabulary that names colors Suru does not paint yet (the diff keys; the syntax keys now resolve into Code Block roles) and omits a few roles Suru derives instead.

## Considered Options

- **A Suru-native format keyed by Suru's roles**: rejected because every built-in theme would need hand transcription, upstream additions could not be taken as files, and users would have to learn a format nobody else uses.
- **OpenCode's version-2 hue-scale format**: rejected because its built-in themes are still written in version 1, and version 2 needs a scale-expansion pipeline whose output Suru would then map down to a much smaller role table.

## Consequences

Roles OpenCode does not name, such as the ANSI palette tool output is mapped through, are derived from the semantic keys by a fixed table rather than declared in the file. A document declaring `version: 2` is rejected whole with a Notice. Because user files pin themselves to this format, a future Suru-native vocabulary would need a migration rather than a replacement.
