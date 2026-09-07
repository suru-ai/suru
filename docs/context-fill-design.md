# Context Fill — issue #198

The user confirmed shared understanding of the design. Implementation has not begun.

## Settled decisions

- Replace the bottom-right cumulative token total with Context Fill, retaining Cost beside it.
- Show `12.4K (6%)`, without a `Context` prefix, in the existing footer style. No warning colors or progress graphic.
- Context Fill is the latest known occupancy of the open Session alone, excluding Subagents. It can decrease after compaction.
- Divide occupancy by the Model's context window, not the Provider's compaction threshold. Round percentages to whole numbers; do not cap values at 100%.
- When capacity is unknown, show occupancy alone. When occupancy is unknown, hide Context Fill. Never substitute cumulative Usage or an invented zero.
- Update whenever a Provider report arrives, including during a Turn. Retain the latest value between reports; do not estimate growth from streamed text.
- Persist the latest snapshot so reopening a Session immediately shows its last known Context Fill until a newer report replaces it.
- Selecting another Model leaves the current Turn's measurement intact. Clear the measurement when a Turn begins with the new Model, until fresh data arrives.
- For Claude, request a native context snapshot after a Turn settles and after compaction, with a bounded timeout. Do not delay Turn completion for the request. Unsupported or failed requests retain the last known measurement, or leave Context Fill hidden if none exists.
- Show known Cost independently when Context Fill is unknown, and Context Fill independently when Cost is absent. Separate them with ` · ` when both appear.
- Under width pressure, drop Cost first, then the context token count, retaining the percentage longest: `12.4K (6%) · $0.42` → `12.4K (6%)` → `6%`. If capacity is unknown, retain the token count instead. Hide the readout when its minimal form cannot fit.

## Findings

The issue's original assertion that this needs display work only is incorrect. The current protocol carries some context capacity but no distinct occupancy snapshot. Existing Usage aggregates consumption and merges capacity by maximum, which cannot represent current Context Fill after compaction or a change to a smaller Model.

- Codex supplies latest context occupancy in native `last.total_tokens`, currently discarded by Suru.
- Copilot supplies `session.usage_info` with `currentTokens` and `tokenLimit`, currently ignored by Suru.
- Claude's official SDK exposes a native `get_context_usage` control request. Its response distinguishes `totalTokens`, `rawMaxTokens` (raw Model window), and `maxTokens` (effective capacity, potentially reduced by a compaction buffer). Compatibility with Suru's supported CLI versions and live behavior still need verification. See the [official response types](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/types.py) and [request implementation](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/_internal/query.py).

## Implementation verification

- Verify native context data and supported-version behavior across all three Providers. Keep Context Fill separate from cumulative Usage and historical Cost.
- Verify reports replace rather than accumulate occupancy, compaction can reduce it, and child Session reports affect only their own Context Fill.
- Verify persistence, Model-change invalidation, and late snapshots so an older request cannot overwrite a newer measurement or restore one invalidated by a Model change.
- Verify missing data, whole-number percentages above 100%, independent Cost rendering, and footer width fallback. Treat a nonpositive capacity as unavailable for division.
- Exercise Claude request failures and timeouts without blocking Turn completion, using injectable short timings in tests.

No open product decisions remain. Native-wire compatibility is an implementation verification item.
