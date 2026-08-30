# Spinner as a draw-time overlay on the memoized transcript

ADR 0007 makes the transcript projection memoized and frame-independent: its cache key is built from the Session snapshot, width, theme, and view state, and the run loop is idle-by-default. An animated Spinner in Activity Markers varies per frame, which collides with both guarantees. We keep the projection frame-independent: it renders every Active Marker as the Spinner's first frame and records which projected lines carry one, and each draw patches the current frame's glyph into those cells after the memoized lines are fetched. Animation is driven by a tick interval that exists only while at least one Spinner is on screen — it sets the dirty flag and is dropped when everything settles, so an idle TUI still schedules zero wakeups.

## Considered Options

- **Frame index in the transcript cache key**: rejected — it invalidates the cache every tick, rebuilding the world at the tick rate, which is exactly the FPS-capped rebuilding ADR 0007 rejected.
- **Animating only non-memoized chrome (status line) and leaving transcript rows static**: rejected — liveness belongs on the row doing the work; the chrome indicator is a supplement, not a substitute.

## Consequences

- Rendering-layer tests see the Spinner's first frame; only the draw-time patch is frame-dependent.
- Anything that adds a new animated surface to the transcript must record its cells in the projection, or the overlay will not reach it.
- The tick period and frame set are hardcoded (candidate settings).
