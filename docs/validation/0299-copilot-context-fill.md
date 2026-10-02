# Copilot Context Fill (#299)

Verified 2026-09-08 against GitHub Copilot CLI **1.0.83**, the pinned
`github-copilot-sdk` **1.0.12-preview.0**, and the official SDK checkout at
`1644e74578db3637bc7527951bac227aabbc0584`.

## Native evidence and denominator decision

The official SDK documents `session.usage_info` as an **ephemeral** Session
snapshot with `currentTokens`, `tokenLimit`, and `messagesLength`:
[Streaming events](https://github.com/github/copilot-sdk/blob/1644e74578db3637bc7527951bac227aabbc0584/docs/features/streaming-events.md#sessionusage_info).
Its usage documentation says the runtime emits this event whenever context size
changes:
[Usage and billing](https://github.com/github/copilot-sdk/blob/1644e74578db3637bc7527951bac227aabbc0584/docs/features/usage-and-billing.md#live-updates-with-sessionusage_info).
The native envelope carries optional `agentId` attribution independently of the
ephemeral flag; Suru's SDK preserves both.

These sources describe `tokenLimit` as the maximum context-window tokens, but do
not say which of a Model's several limits it is. The original verification made no
live model call, so it left capacity unknown.

## Live denominator check (2026-10-02)

Rechecked against GitHub Copilot CLI **1.0.85** and `github-copilot-sdk`
**1.0.15-preview.3** with `examples/copilot_context_probe.rs`, which runs one
short authenticated Turn per Model. It compares `session.usage_info`, `assistant.usage`,
the `models.list` catalog limits, and `session.metadata.getContextAttribution`:

| Model | catalog prompt / window | `tokenLimit` | `maxPromptTokens` | attribution `promptTokenLimit` / `limit` |
|---|---|---|---|---|
| gpt-5-mini | 128000 / 264000 | 128000 | 128000 | 128000 / 128000 |
| claude-haiku-4.5 | 128000 / 144000 | 128000 | 128000 | 128000 / 128000 |
| gpt-5.4-mini | 272000 / 400000 | 272000 | 272000 | 128000 / 128000 |
| claude-sonnet-5 | 936000 / 1000000 | 200000 | 200000 | 128000 / 128000 |

`tokenLimit` is the window the Session's Model is served at. It follows the context
tier (claude-sonnet-5's default tier is 200000, not its 1M catalog window) and always
equals the limit Copilot enforces on the prompt. The catalog's
`max_context_window_tokens` is larger than anything a prompt may reach. Used as a
denominator, it would understate fill: Copilot compacts well before the gauge
would get there. The attribution RPC is no source either. It is not told the
Model's limits and answers the runtime default of 128000 whatever the Model.
`session.metadata.contextInfo` merely echoes limits the caller passes.

So **`tokenLimit` is the capacity**, when positive. It matches what the other
Providers report for the same Models: Claude's `rawMaxTokens` is 200000 for
Sonnet at its default tier, and Codex's raw window for gpt-5.4-mini is 272000.
Zero, negative, or missing `tokenLimit` leaves capacity unknown. Subagents report
their own `tokenLimit`, which the recorded fixtures show differing from the
parent's (200000 against 128000), so each Session's capacity comes from its own
reports.

A Context Breakdown, read from that same attribution RPC, takes its categories
from it but its window from here: the main conversation's latest positive
`tokenLimit`, forgotten when a Turn begins under another Agent Selection until
Copilot reports one again. Its reserve stays unknown, since the attribution's
buffer is measured against the default rather than that window.

## Adapter behavior

The existing lossless SDK timeline drain observes ephemeral context reports,
accepting a nonnegative integer `currentTokens` even if other native fields are
missing. Missing, negative, or malformed occupancy is ignored. Zero is a real
measurement. The generic Context Fill contract handles client updates, replacement,
persistence, presentation, and parent/child isolation. Reports attributed to an
unknown child remain unrouted; settled known children can still receive context.

The drain allocates an increasing sequence and captures the last delivered
Prompt's Suru Turn ID before a report waits in the projection queue. Model
selection and skill expansion keep the old identity until the new Prompt is ready
for delivery. Reports already queued under an older Prompt therefore retain their
old identity when a later Model invalidates it. Native events have no Model or
Turn identifier, so correlation relies on Copilot's ordered Session timeline;
there is no inference from cumulative metering.

Continuation IDs are owned by orchestration. Once preceding native content has
opened a Continuation, context from that same captured Prompt generation binds
in stream order to the Continuation, including after its idle. An older Prompt's
queued report is never rebound to a newer Prompt generation. Context reports do
not themselves open Continuations. Admission immediately disables Continuation
rebinding, including when Model selection fails before the Prompt can be sent.
If native `session.send` rejects after readiness, abandoning startup clears the
context origin. This also drops owning observations already queued during the
failed send, without changing the previously accepted Session snapshot. Child
reports remain independently attributable.

## Validation

The existing Unix-gated scripted CLI integration harness verifies ephemeral live
updates, positive, missing, and zero capacity, invalid/missing occupancy, replacement after idle,
parent/child isolation, unknown-child rejection, settled-child zero, restart
persistence, and unchanged Usage and Cost. A second native timeline exercises
context during a Continuation and after its settlement. A third exercises failed
Model startup after a Continuation: a later child update confirms the stale parent
report crossed projection before checking that invalidated context stayed absent.
A fourth rejects native `session.send` after a successful Model switch, emitting
context both before and after the rejection reply. Its child-update barrier
confirms neither queued nor later context revives the invalidated reading.
The pre-existing metering
test explicitly verifies that Usage alone leaves Context Fill unknown.

Portable shared Session tests cover Model-change invalidation and old/out-of-order
report rejection, child ownership, persistence, and replacement. Existing footer
rendering tests cover the shared formatting and width policy. No separate Copilot
rendering path or Setting is introduced. Fixture waits use the existing release
barrier with a 10ms polling interval; workspace and state paths use temporary
platform-native directories.

Initial combined checks: `cargo nextest run --test copilot_integration --test
session_integration --lib --status-level fail` passed **708/708** tests (including
shared footer tests), with `src/lib.rs` touched inside the shared cargo lock to
force this worktree's library rebuild. `cargo fmt --check` and `git diff --check`
also passed. All builds used `/home/jake/Projects/suru/target` under
`/tmp/suru-198-cargo.lock`.

After the rejected-send follow-up, a forced rebuild passed all **58/58** Copilot
integration and shared Context Fill tests using `--test copilot_integration --test
session_integration -E 'binary(copilot_integration) | test(context_fill)'`.
Formatting and diff checks passed again.
