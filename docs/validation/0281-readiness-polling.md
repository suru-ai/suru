# Readiness polling validation for #281

## Change

Startup waits 5, 10, 20, 40, then at most 50 ms between unsuccessful probes. The schedule is local to one `ensure_server` call and does not reset on lifecycle changes, child exit, or election/replacement retries. A slow server therefore settles at the previous maximum rate of 20 probes/second, with only a bounded initial burst (at most three additional probes compared with fixed 50 ms polling over the same elapsed time, excluding probe cost). Each wait ends no later than the existing absolute startup deadline; expiration retains the last probe error and bounded log tail. An already-expired startup budget issues no probe.

`ManagedClientConfig::with_readiness_polling` injects initial and maximum intervals for tests. Initial intervals clamp to at least 1 ms and the cap to at least the initial interval. This is internal configuration, independent of recovery backoff and user-facing Settings. No IPC or unauthenticated readiness shortcut was added: every successful launch still passes the existing authenticated identity, lifecycle, protocol, and build checks.

## Measurements

Linux, optimized release build, warm filesystem caches, ten alternating fixed/adaptive pairs. Every fresh launch used a temporary state/data/config root and a new server process, with Codex, Copilot, and Claude disabled in `suru.jsonc`. Reuse immediately followed its fresh launch, and each isolated server was stopped afterward. Timings cover the public `start_server` call, including executable identity and authenticated probing; they exclude TUI startup, terminal color negotiation, and event stream attachment.

The before comparison used the same executable and library with `with_readiness_polling(50 ms, 50 ms)`, reproducing the previous polling cadence on successful launches. This isolates cadence from unrelated changes since the issue's original measurements; it is not a comparison with that older revision. The adaptive comparison used the production defaults.

Temporary instrumentation counted launcher probes (including missing-descriptor attempts) and timestamped the server immediately before publishing its Ready lifecycle. A temporary integration harness timestamped `start_server` returning, subtracting the server timestamp for actual readiness-to-recognition. The timestamps used the same host's wall clock; total call durations used monotonic time. Instrumentation and the harness were removed after measurement. Probe stderr writes and the readiness timestamp add a small measurement overhead to both cases. Fresh-server polling phase and scheduler variance explain the spread; these local samples are not a cross-platform performance guarantee.

Medians:

| Metric | Fixed 50 ms | Adaptive |
| --- | ---: | ---: |
| Actual fresh-server readiness to recognition | 39.53 ms | 24.89 ms |
| Total fresh-server startup | 57.88 ms | 44.92 ms |
| Total reused-server startup | 5.42 ms | 5.52 ms |
| Fresh-start probe count | 2 | 4 (range 3–4) |
| Reused-start probe count | 1 | 1 |
| Controlled readiness immediately after first probe to recognition | 58.14 ms | 11.73 ms |
| Controlled readiness probe count | 2 | 2 |

The controlled HTTP fixture captures a Starting response and immediately changes its lifecycle to Ready before returning that response. Its timestamp is taken at that transition, independently of the launcher. This measures the between-probe delay plus the subsequent authenticated request, without subprocess startup cost. The observed improvement is 46.41 ms in the controlled case and 14.64 ms for the median real fresh server; the real startup improvement cannot all be attributed to polling.

Raw samples in milliseconds, in run order:

- Fixed fresh total: 58.70, 57.06, 57.43, 56.70, 56.82, 59.72, 62.23, 61.72, 57.07, 58.34.
- Adaptive fresh total: 22.98, 26.05, 24.05, 46.28, 45.27, 45.39, 44.58, 43.57, 45.56, 45.48.
- Fixed fresh recognition: 39.99, 40.13, 38.76, 36.03, 39.08, 42.25, 45.16, 43.15, 37.64, 39.03.
- Adaptive fresh recognition: 5.86, 9.52, 6.79, 27.77, 25.20, 21.94, 25.80, 25.07, 24.89, 24.89.
- Fixed reused total: 5.31, 5.41, 5.51, 6.49, 5.23, 5.42, 6.35, 5.33, 5.12, 8.96.
- Adaptive reused total: 5.36, 5.53, 5.43, 6.36, 6.47, 6.46, 5.94, 5.23, 5.51, 5.50.
- Fixed controlled recognition: 57.07, 56.95, 56.86, 58.08, 58.69, 56.94, 58.31, 61.42, 58.21, 61.22.
- Adaptive controlled recognition: 11.87, 11.58, 11.88, 12.64, 11.59, 12.71, 11.74, 11.51, 11.64, 11.72.

## Regression coverage

New tests exercise the public `start_server` boundary through authenticated loopback HTTP: readiness immediately after a probe, a 150 ms deadline with a 1,000 ms injected wait, and a server that stays Starting for its 180 ms startup budget. They verify prompt recognition, no probe after deadline, bounded probe rate/backoff, and retained failure context. The initial prompt-readiness test failed on the original implementation (57.90 ms recognition) and passed with adaptive polling. Review replaced its tight 45 ms wall-clock limit with a comparison against an injected fixed 250 ms cadence, leaving substantial scheduler headroom while still requiring the default to recognize readiness in less than half the fixed-cadence time.

The CLI integration suite also covers child failure and bounded log tails, lost elections, concurrent clients, replacement, stale/malformed descriptors, inaccessible endpoints, transitional and failed lifecycle states, authenticated identity, protocol compatibility, and channel isolation.

Validation runs on Linux. The implementation uses portable Rust/Tokio timing and the tests use temporary paths and loopback TCP without platform-specific helpers. Windows and macOS execution remains to be verified on native runners; neither native cross-compilation toolchain is available here.

Completed checks:

- `cargo check --all-targets --locked`: passed.
- `cargo nextest run --test cli_integration`: 37 passed, 1 skipped.
- `cargo nextest run --locked`: 1,670 passed, 4 skipped (full suite).
- After review widened timing-test headroom, the three readiness regression tests were rerun with Nextest and passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Independent standards and spec reviews found no remaining implementation findings. Standards review's timing-test reliability concern was addressed; spec review identified native Windows/macOS execution as the outstanding validation requirement.
