# Idle TUI rendering with a memoized transcript projection

Rendering used to rebuild the entire transcript (markdown parse, line wrapping, layout) from the Session snapshot on every terminal event, which pegged the CPU under input bursts and made a frame cost ~240ms in dev builds on a medium session. We keep ratatui's immediate-mode drawing but make the frame's inputs retained: `tui::transcript::TranscriptCache` memoizes the projected transcript per Session revision and width, reusing per-item rendered lines so a streaming append only re-renders the item it touched, and each frame hands ratatui only the viewport-sized window of lines. The run loop is idle-by-default: it redraws only when an event actually changed state (a dirty flag; translated commands, session/managed events, and resizes set it, unhandled input does not) and drains already-pending input before drawing so a burst costs one frame. This mirrors how opencode's opentui renderer schedules frames (dirty-flag one-shot frames, per-block markdown reuse, viewport culling) adapted to immediate-mode ratatui.

## Considered Options

- **Reactive/retained renderer (opencode-style)**: rejected as a wholesale replacement; ratatui's buffer diffing already gives cheap terminal writes, so memoizing the widget-model construction gets the same effect with far less machinery.
- **Throttling frames instead of caching**: an FPS cap alone would still rebuild the world at the cap rate and adds latency; caching makes frames cheap enough that a cap is unnecessary today.

## Consequences

- Mouse capture is enabled with hand-written escape sequences (`1000h`/`1006h` only) instead of crossterm's `EnableMouseCapture`, which also enables any-motion tracking (`1003h`) and floods the input stream with pointer-move events nothing consumes. Don't "simplify" this back to `EnableMouseCapture` while hover/motion has no consumer; if hover UI is ever added, pair `1003h` with an O(1) hit test and identity-change filtering like opentui's. Suru owns and parses the VT input stream on every platform, including Windows, so the same reporting modes and filtering behavior apply everywhere.
- `apply_update` now mutates the snapshot in place (no defensive clone per streamed update); on error the snapshot must be discarded, which every caller already does. Callers needing atomicity clone first (`sessions.rs` does).
- The cache is keyed by revision plus a generation counter bumped on snapshot replacement, so correctness never depends on revisions being unique across re-attachments.
- Anything that changes transcript rendering inputs outside the Session snapshot, provisional prompts, or width must be added to the cache key, or stale frames will render.
- `tests/transcript_perf.rs` (`--ignored`) is the timing harness that guards this: warm/scroll/streaming frames should stay well under a millisecond in release builds.
