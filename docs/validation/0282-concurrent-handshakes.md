# Concurrent managed handshakes validation for #282

## Change and cancellation

The lifecycle and Session catalog HTTP handshakes start in one `tokio::try_join!`. Both must pass their existing HTTP status validation before the managed connection is adopted. Either rejection, transport failure, or timeout returns the original stream-specific error and drops the sibling future, including an already-opened response. There are no detached handshake tasks. Both use the same absolute startup deadline, including time already spent ensuring the server; startup errors still include the bounded server log tail.

The shared helper serves initial attachment, crash recovery, and replacement attachment. Stream consumption is unchanged: the initial catalog snapshot hydrates before `Connected`, then lifecycle consumption delivers Settings.

## Measurements

Before: `0ac854a885c0b7269a7e3f308705dfc460990e83`. After: this change. Linux debug builds, `cargo build --locked`, warm filesystem caches, reused real loopback servers, empty Session catalogs, all three Providers disabled. Each binary had its own isolated temporary state/data/config roots and server process; both servers were stopped afterward. No production state or configuration was used.

A temporary Python standard-library harness launched each TUI in a 160-column by 40-row PTY. It timestamped process launch and the first cursor-show sequence after the final OSC color query. It immediately answered all 16 palette queries plus foreground/background queries, and answered cursor-position queries separately. Each TUI exited with Ctrl+C after its first frame. Each scenario used 20 samples per binary, alternating before/after launch order, following two warm-up pairs.

For handshake measurements, an authenticated HTTP pass-through proxy forwarded the real server's responses and timestamped the first SSE request's arrival and the second SSE response's headers being flushed. This is the wire establishment interval; it excludes initial client/server readiness work and processing after the second headers arrive. The delayed fixture held each upstream SSE response's headers for 50 ms independently. The zero-delay proxy shows real loopback handshake costs with the same instrumentation. The direct scenario removed the proxy for end-to-end practical impact. Proxy and PTY scheduling add measurement overhead, so sub-millisecond differences should not be extrapolated to other machines or platforms.

Medians in milliseconds:

| Scenario and metric | Sequential | Concurrent |
| --- | ---: | ---: |
| 50 ms per handshake: handshake interval | 103.39 | 50.95 |
| 50 ms per handshake: launch to first frame | 131.77 | 79.46 |
| Zero-delay proxy: handshake interval | 1.29 | 0.52 |
| Zero-delay proxy: launch to first frame | 26.42 | 25.81 |
| Direct real loopback: launch to first frame | 25.40 | 25.77 |

The controlled handshake interval improves by 52.44 ms and first frame by 52.30 ms, demonstrating overlapping waits. The zero-delay handshake difference is 0.77 ms. Direct first-frame ranges overlap (23.70–28.16 ms before, 23.84–28.44 ms after), with no measurable practical first-frame improvement in these samples. No production latency saving is claimed from the deliberately delayed fixture.

## Regression coverage

The integration seam is `ManagedClient::connect` against the existing readiness HTTP fixture, as specified by the issue. The concurrency tracer test failed on the sequential implementation because the server never observed the catalog request while holding the lifecycle response; it passed after the join was introduced.

Coverage includes:

- Both requests observed before either response is released; either response may finish first, and one success alone cannot adopt a connection.
- Rejection of either stream with its sibling waiting or already open; exact stream context and HTTP status retained, sibling closure observed by the server.
- A stalled handshake in either stream, bounded startup diagnostics, and server-observed closure of the pending request and successful sibling response.
- Abrupt TCP closure before either stream's headers, accurate transport-error context, and closure of the other pending socket.
- A 500 ms startup budget with 300 ms already spent waiting for Ready, proving stream opening does not receive a fresh budget.
- Withheld catalog snapshot body, proving `Connected` and Settings wait for hydration even after both HTTP responses open.
- Existing CLI coverage for recovery, replacement, Settings ordering, protocol failures, and terminal restoration.

All new regression tests use portable Tokio/Axum/loopback TCP and temporary absolute paths. Execution was available only on Linux; Windows and macOS native validation remains outstanding.

## Checks

- `cargo check --all-targets --locked`: passed.
- `cargo nextest run --test cli_integration`: 42 passed, 1 skipped.
- After review, all six handshake/hydration regression tests passed again.
- `cargo nextest run --test claude_integration --no-fail-fast`: 68 passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo nextest run --locked --no-fail-fast`: 1,681 passed, 4 skipped (normal `/tmp`, 34.54 seconds).

Independent code-review results: Standards has zero outstanding findings after strengthening the successful-sibling streaming signal and centralizing the fixture stream/name mapping. Spec has no implementation correctness or scope findings; its one partial requirement is native Windows/macOS execution.

An initial CLI-suite run hit `/tmp`'s quota in an existing executable-copy test. The first successful CLI rerun used a workspace temporary directory. A full-suite run there exposed six unrelated rendering fixtures whose expected text was truncated by the longer paths; the affected fixtures pass with normal `/tmp` paths. Removing the benchmark binary copy relieved the quota, and the executable-copy test also passed in `/tmp` before the final full run.

Full-suite execution also twice exposed a race in the existing Claude Skill-steer test: a Turn becoming Active does not imply that the CLI has received its initial Prompt, so its request log could still contain zero user inputs when the test asserted one. The fixture now emits an open text block and the test waits for that Agent Message through the existing Session feed before exercising rejection. The original no-extra-native-input assertion remains; no production Claude behavior changed and no sleep was added.
