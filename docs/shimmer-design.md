# Shimmer motion

Shimmer labels move at a constant speed measured in terminal columns, using
“Working” as the baseline. Its sweep remains 1.5 seconds. The highlight keeps
its five-column half-width and each sweep is followed by one second at rest.

The highlight center travels 16 columns across “Working”, including travel
beyond both ends, giving 93.75 milliseconds per column. For a nonempty label
of width W, the sweep lasts (W + 9) × 93.75 milliseconds. “Loading” therefore
also takes 1.5 seconds; “Waiting for subagents” takes 2.8125 seconds.

A label change restarts the sweep from the left. The existing animation tick
provides the clock; the label remembers its starting frame in client view
state. Styling stays outside the memoized Transcript projection, consistent
with ADR 0009. Elapsed time and interruption guidance do not shimmer.

Extended grapheme clusters stay intact and are styled at their starting
terminal column. Timing uses the full label width before clipping, so resizing
the terminal does not change the speed or cycle of a label.

This is a reversible presentation change, so it does not introduce a Setting,
new glossary term, or architectural decision record.
