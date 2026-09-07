## Problem Statement

The bottom-right Session footer shows cumulative token Usage, including Subagent Usage. That figure grows across Turns and cannot tell the user how much of the open Session's context window is occupied. A user needs to distinguish current context occupancy from accumulated consumption, especially after compaction or a Model change.

## Solution

Replace the footer's cumulative token total with Context Fill: the latest known context occupancy of the open Session against its Model's context window. Show `12.4K (6%)` without a label, retaining known Cost beside it as `12.4K (6%) · $0.42`. Use the existing footer style.

The readout follows Provider reports, survives reopening the Session, and can decrease after compaction. Unknown data remains absent. When space is tight, drop Cost first, then the token count, preserving the percentage longest.

## User Stories

1. As a user, I want current Context Fill in the bottom-right footer, so that I can assess context occupancy while composing a Prompt.
2. As a user, I want Context Fill to replace the cumulative token total, so that the footer answers how full the context is now.
3. As a user, I want a compact token count and percentage without a text prefix, so that the readout occupies little space.
4. As a user, I want the percentage measured against the Model's context window, so that it describes context occupancy consistently.
5. As a user, I want the compaction threshold excluded from that denominator, so that fill does not silently mean proximity to compaction.
6. As a user, I want the open Session's own Context Fill, so that Subagents do not inflate its occupancy.
7. As a user viewing a child Session, I want that child's own Context Fill, so that I can understand its independent context.
8. As a user, I want Context Fill to update when Provider reports arrive during a Turn, so that available measurements appear promptly.
9. As a user, I want Context Fill to decrease when a newer report reflects compaction, so that released capacity is visible.
10. As a user, I want the latest measurement retained between reports, so that the readout does not flicker or invent growth.
11. As a user, I want known occupancy shown without a percentage when capacity is unknown, so that partial information remains useful.
12. As a user, I want unknown occupancy hidden, so that missing information is not mistaken for zero.
13. As a user, I want cumulative Usage excluded as a fallback, so that the readout retains one meaning across Providers.
14. As a user reopening a Session, I want its last known Context Fill immediately, so that I need not start a Turn just to see the previous measurement.
15. As a user selecting another Model during a Turn, I want the working Turn's measurement retained, so that the readout still describes the Model doing the work.
16. As a user starting a Turn with a different Model, I want the old measurement cleared until fresh data arrives, so that a previous Model's capacity is not misrepresented.
17. As a user, I want known Cost retained beside Context Fill, so that accumulated Cost remains visible.
18. As a user, I want known Cost shown even when Context Fill is unknown, so that missing context data does not conceal Cost.
19. As a user, I want Context Fill shown even when Cost is absent, so that missing pricing does not conceal context data.
20. As a user with a narrow terminal, I want Cost dropped before Context Fill, so that the primary readout survives width pressure.
21. As a user with still less space, I want the percentage retained without the token count, so that occupancy remains readable.
22. As a user with unknown capacity, I want the token count retained as the compact fallback, so that available information remains visible.
23. As a user, I want a readout that cannot fit hidden, so that fragments do not present misleading numbers.
24. As a user, I want whole-number percentages, including reported values above 100%, so that formatting does not conceal the measurement.
25. As a user, I want the existing footer style without new warning colors or graphics, so that this change remains a compact informational readout.
26. As a Claude user, I want native context snapshots requested after Turn settlement and compaction, so that Context Fill can refresh without relying on cumulative result Usage.
27. As a Claude user, I want unavailable or failed context requests to preserve the previous reading, so that optional reporting does not disrupt my Session.
28. As a Claude user, I want Turn completion independent of context-query latency, so that the readout does not delay the work's settlement.
29. As a user, I want cumulative Usage and historical Cost accounting preserved, so that changing the footer does not rewrite consumption or pricing facts.

## Implementation Decisions

- Introduce a distinct, Provider-neutral Context Fill snapshot in the shared Session contract, carrying known occupancy and optional valid context-window capacity. Keep it separate from cumulative Turn Usage and Cost; support Codex, Copilot, Claude, and future Providers through the same interface.
- Extend Provider event projection, Server Session state, persistence, client updates, and footer presentation as necessary. Reuse the existing typed footer extension seam rather than creating a generic rendering interface.
- A Context Fill report replaces the previous measurement; it is not summed across Turns, merged by maximum, or rolled up from Subagents. Attribute child reports to their own Sessions.
- Use the raw Model context window as the denominator, not a compaction threshold or capacity reduced by a compaction buffer. Treat missing or nonpositive capacity as unavailable for division. Round percentages to whole numbers without clamping at 100%.
- Consume Codex's latest context reading from native `last.total_tokens`, with its reported model context window. Keep cumulative native totals on the existing Usage path.
- Consume Copilot's native `session.usage_info` occupancy and capacity reports, respecting Session/Subagent attribution. Verify the native capacity's semantics against the selected raw-window definition rather than silently substituting an effective compaction limit.
- For Claude, use the native `get_context_usage` control request with `totalTokens` for occupancy and `rawMaxTokens` for the Model window. Do not use `maxTokens`, which may reflect an effective capacity reduced by the compaction buffer. Verify supported CLI behavior during implementation.
- Request Claude snapshots after Turn settlement and compaction with a bounded, injectable timeout. These requests must not delay Turn completion. Unsupported requests, errors, or timeouts retain the last known snapshot, or leave the readout hidden when none exists. Do not infer occupancy from aggregate result Usage.
- Apply valid reports whenever they arrive, including during a Turn, and retain the latest snapshot between reports. Do not estimate token growth from streamed text.
- Persist the latest snapshot and expose it when the Session is reopened. A Session with no stored measurement begins with unknown Context Fill.
- Keep the current Turn's measurement when Agent Selection changes. Invalidate it when a Turn begins with a different Model. Ensure late responses cannot overwrite newer snapshots or revive a measurement invalidated by a Model change.
- Render a compact count and parenthesized percentage with no prefix. Show occupancy alone when capacity is unavailable and hide Context Fill when occupancy is unavailable. A genuinely reported zero is distinct from unknown.
- Retain the existing Cost semantics, including subtree accounting and existing rules for zero or absent Cost. Render Cost independently of Context Fill and separate the two with ` · ` only when both appear. Respect the decision to freeze a Turn's Cost when recorded.
- Degrade under width pressure in this order: full Context Fill with Cost, full Context Fill without Cost, percentage alone. If capacity is unknown, retain the token count instead of a percentage. Hide the readout if its minimal form cannot fit. Preserve the existing footer's layout responsibilities.

## Testing Decisions

- Test externally observable behavior at existing integration and rendering seams. Assert what a Client receives and what a user sees, rather than private reducers, helper call counts, or storage layout. No new test-only interface is proposed.
- Use the existing Session integration harness with its controlled Provider and client-visible snapshots/streams as the primary seam. Prior art includes Usage attribution and subtree tests, Model selection tests, and storage/restart tests.
- At that seam, cover replacement rather than accumulation, decreases after compaction, independent parent/child measurements, live updates, persistence and reopening, missing occupancy/capacity, Model-change invalidation, and late-report rejection. Confirm cumulative Usage and frozen Cost remain independent.
- Extend existing scripted Provider integration tests to exercise native wire input through the runtime into observable Session state. Prior art includes Codex metering tests and Claude/Copilot Turn, interruption, and resume fixtures. Cover Codex latest versus cumulative totals, Copilot context reports and child attribution, and Claude raw versus effective capacity.
- Verify Claude requests after settlement and compaction, unsupported responses, errors, timeout behavior, and out-of-order responses. Demonstrate that waiting for context data does not hold a Turn open.
- Extend existing TUI rendering tests for composer footer metering and active Session updates. Assert label-free formatting, whole-number percentages above 100%, genuine zero versus unknown, independent Cost, and each width fallback.
- Keep tests portable across Windows, macOS, and Linux. Existing shell-based Provider fixtures are prior art, not permission to introduce unconditional POSIX dependencies. Use portable fixtures or appropriate platform gates, with shared behavior covered by the portable Session seam. Use platform-correct paths.
- Run relevant suites with `cargo nextest run`. Inject millisecond-scale timing values for timeout tests; do not wait out production-scale delays.

## Out of Scope

- Implementing the feature during this specification task.
- Changing cumulative Usage, Cost calculation, Cost Basis, or historical Cost records.
- Aggregating Context Fill across a Subagent subtree.
- Displaying remaining capacity or proximity to the compaction threshold instead of context-window fill.
- Adding warning colors, progress graphics, a context-details panel, new Settings, or a user-selectable display mode.
- Estimating occupancy from streamed text, cumulative Usage, or speculative fallback tokenization.
- Changing Provider compaction behavior or triggering compaction from the footer.
- Adding a plugin loader or public plugin API, or relocating the readout to the Sidebar.

## Further Notes

This specification replaces the original design sketch for issue #198. The user confirmed the complete design during the interview.

The original issue's claim that only display work is needed is incorrect: the current shared contract lacks a distinct occupancy snapshot, Codex's latest reading is discarded, Copilot context events are ignored, and Claude currently lacks context reporting. Provider plumbing and Session persistence are part of this feature.

Claude's native request and response fields are documented in the [official SDK request implementation](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/_internal/query.py) and [official SDK types](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/types.py). Supported-version compatibility and live behavior remain implementation verification items; they have not been established through authenticated runtime testing.
