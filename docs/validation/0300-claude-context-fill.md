# Claude native Context Fill (#300)

Implementation of [#300](https://github.com/jake-tucker/suru/issues/300), under
[#198](https://github.com/jake-tucker/suru/issues/198).

## Native compatibility evidence

On 2026-09-08, the installed Linux CLI reported `2.1.260 (Claude Code)` from
`claude --version`. A bounded, read-only stream-json process was launched with:

```text
claude --print --input-format stream-json --output-format stream-json --verbose --setting-sources ''
```

Its stdin received one request, with no user prompt:

```json
{"type":"control_request","request_id":"suru-context-validation","request":{"subtype":"get_context_usage"}}
```

The correlated `control_response` had subtype `success`. These fields were
extracted from its `response.response` payload:

```json
{"totalTokens":14144,"rawMaxTokens":1000000,"maxTokens":1000000,"model":"claude-opus-5[1m]"}
```

The probe was bounded to 15 seconds and the process terminated after the reply.
It requested no model generation. This establishes actual request compatibility
for the installed 2.1.260 CLI, including field shape; it does not establish live
post-Turn or compaction timing, nor compatibility for every older CLI. The
existing 2.1.237 suggested version is unchanged. Optional unsupported requests
leave the Session usable and retain its last accepted reading.

The official Python SDK's
[query implementation](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/_internal/query.py)
issues `get_context_usage`. Its
[ContextUsageResponse types](https://raw.githubusercontent.com/anthropics/claude-agent-sdk-python/main/src/claude_agent_sdk/types.py)
describe `totalTokens` as current occupancy, `rawMaxTokens` as the raw Model
window, and `maxTokens` as effective capacity potentially reduced by the
compaction buffer. These references were checked on 2026-09-08. The live probe
had equal capacities; scripted native tests deliberately use unequal values to
verify Suru reads `rawMaxTokens` exclusively.

## Implementation and boundaries

- A Session requests a snapshot after owning Turn settlement and owning
  `system/compact_boundary`, without awaiting the request in conversation
  projection. `with_context_request_timeout` injects the default five-second
  bound. The transport bounds sending and receiving together and unregisters
  pending requests on timeout or cancellation.
- Each query captures its explicit Prompt Turn ID, concrete Model identity from
  `system/init` (or the selected ID if unavailable), and sequence before starting
  asynchronous work. The optional `[1m]` suffix is normalized when comparing
  native Model identities. A different response Model is rejected.
- Missing/invalid occupancy produces no report. Missing, zero, negative or
  malformed raw capacity produces a count-only snapshot. Cumulative result
  Usage, effective capacity and pre-compaction token counts are never fallbacks.
  Unsupported responses, errors and timeouts produce no Context Fill mutation.
- New Prompt startup disables query eligibility before fallible Skill lowering
  or child setup. Eligibility returns only once the selected child is ready,
  and is withdrawn if sending fails. An old process cannot report under a new
  Turn ID when setup fails.
- Shared orchestration creates late-output Continuations without calling
  `start_turn`. At ordered report delivery, the Provider binds a query to that
  Continuation only if its captured Prompt generation is still current and
  eligible. A new Prompt clears that permission. A Provider-side sequence gate
  also rejects older replies across Continuation boundaries, complementing the
  shared per-Turn gate. Usage-only and questionnaire output follow the same
  continuation rule as transcript text.
- Native `get_context_usage` has no child-conversation selector. Its report is
  attributed only to the owning Session; a child's compaction never triggers
  a parent query. Children hosted in this process have unknown Context Fill,
  rather than inheriting a parent's snapshot or approximating it from Usage.
  Separately hosted Claude Sessions use their own native process and query
  channel. Parent/child persistence and display remain on the shared contract.

## Verification

The Unix-gated scripted Claude integration harness covers live client updates,
settlement and compaction triggers, decreasing occupancy, raw versus effective
capacity, zero/count-only/invalid data, preserved Usage and Cost, restart
persistence, unsupported/error/timeout retention, pending queries across Turns,
out-of-order replies, Model changes, failed Model setup, child attribution and
late-output Continuations. Timeout tests inject millisecond bounds; no production
query timeout is waited out.

The shared portable Session and TUI harnesses verify persistence, independent
child measurements, Model invalidation, formatting, Cost and width fallbacks.
No Claude-specific rendering path or platform-specific production code is added.
Shell fixtures remain under the existing `cfg(unix)` integration target; paths
come from temporary directories. Validation here ran on Linux, not Windows or
macOS.

Validation command (shared target serialized across implementation worktrees):

```sh
CARGO_TARGET_DIR=/home/jake/Projects/suru/target cargo nextest run \
  --lib --test claude_integration --test session_integration --test tui_rendering \
  -E 'binary(claude_integration) | test(provider::claude) | test(context_fill) | test(composer::)' \
  --no-fail-fast
```

Result: **181 passed**, including all 80 Claude integration tests (12 new Context
Fill cases), Claude library tests, shared Context Fill tests, and composer
rendering tests. `cargo fmt --check` and `git diff --check` also passed.
