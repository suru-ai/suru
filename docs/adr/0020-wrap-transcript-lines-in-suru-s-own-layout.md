# Wrap Transcript lines in Suru's own layout

The Transcript wraps every projected line with Suru's own text layout, extended to styled lines, and draws the wrapped rows itself rather than handing unwrapped lines to ratatui's `Paragraph` with `Wrap`. A Text Selection copies the text as it was written, with soft wraps undone and Suru's decorations skipped, so a screen cell has to resolve to a character offset in the line behind it; that is only trustworthy when the wrap that decided where the rows break is the same wrap that answers the hit test. The composer and the Transcript's user Messages already wrap this way for the same reason: caret, height, and drawn rows must agree.

## Considered Options

- **Keep `Paragraph` wrapping and reimplement its break rules beside it for hit-testing**: rejected because two wrappers that must agree drift apart on exactly the cases that matter — wide characters, long tokens, trailing whitespace — and every ratatui upgrade reopens the question.
- **Copy painted cells and join rows heuristically**, adding a space where a row ends short of its width: rejected because it cannot tell a soft-wrapped URL or code line from two lines that were written apart, and it cannot skip decoration.

## Consequences

The projection records, for every wrapped row, which line it belongs to and where in that line it begins, and it records per line whether it continues a line the oversize cap split and per span whether it is chrome. The Transcript's draw path owns its own scrolling into a partially visible line instead of using `Paragraph`'s scroll offset. Any surface that wants selectable wrapped prose wraps the same way; single-row surfaces keep drawing as they do and copy their painted row.
