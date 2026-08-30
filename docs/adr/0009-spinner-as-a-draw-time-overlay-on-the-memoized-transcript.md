# Animate transcript surfaces as draw-time overlays

ADR 0007 makes the transcript projection memoized and frame-independent: its cache key is built from the Session snapshot, width, theme, and view state, and the run loop is idle-by-default. Animated presentation in the Transcript varies per frame, which collides with both guarantees. We keep the projection frame-independent in two ways. It renders a stable placeholder and records the projected cells carrying an Active Activity Marker, then each draw patches the Spinner's current glyph into those cells. The Working Indicator is transient tail presentation rather than projected Transcript content, so each draw appends it after the memoized rows and computes its shimmer styles there. Animation is driven by a tick interval that exists only while work that can animate is present — it sets the dirty flag and is dropped when nothing can animate, so an idle TUI still schedules zero wakeups.

## Considered Options

- **Frame index in the transcript cache key**: rejected — it invalidates the cache every tick, rebuilding the world at the tick rate, which is exactly the FPS-capped rebuilding ADR 0007 rejected.
- **Animating only non-memoized chrome and leaving transcript rows static**: rejected — liveness belongs alongside the work it describes, including both Active Activity rows and the Working Indicator at the Transcript tail.

## Consequences

- Projection tests see stable Activity Marker placeholders; only their draw-time patch is frame-dependent.
- Animated persisted Transcript content must record the cells its overlay patches. Transient tail presentation may instead stay outside the projection and its cache key.
- Animation timing and frame definitions are hardcoded pending a coherent motion policy across the TUI.
