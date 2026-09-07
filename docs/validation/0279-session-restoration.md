# Session restoration (#279)

Measured on Linux on 2026-09-07, with release builds, warm filesystem caches, and three fresh server processes per case. The final measurements ran sequentially after builds and tests finished. The baseline is `e4a4654371ad1bb5964def7a053b0f207daa5421` with the same debug timing event added; the comparison changes only Session restoration. These measurements do not include lazy history hydration work from #280.

Each run uses disposable absolute state/data/config roots, disables all three Providers and remote serving, lets the binary create/migrate SQLite, then seeds synthetic Session rows. Roots have no history; tree fixtures have four Turns per Session, disjoint historical intervals, one open leaf Turn, and reported output Usage. The authenticated listing count and tree Usage total are checked after every start. No user database or configuration is used.

Times below are medians in milliseconds. Projection measures the Working/Usage derivation only. Store restoration measures `SessionStore::new`, including record/channel construction and projection, but excludes SQLite loading and writer initialization. Readiness measures subprocess launch through successful `suru server start`, including loading, executable/launcher work, and readiness polling. These are local observations, not CI thresholds; small readiness differences are dominated by polling and scheduling.

| Shape | Sessions | Projection before → after | Store restoration before → after | Readiness before → after |
| --- | ---: | ---: | ---: | ---: |
| roots | 0 | 0.002 → 0.002 | 0.018 → 0.017 | 28.146 → 28.941 |
| roots | 1,000 | 3.493 → 0.044 | 10.692 → 7.085 | 49.541 → 51.115 |
| roots | 5,000 | 89.077 → 0.462 | 122.362 → 32.062 | 181.846 → 92.321 |
| roots | 10,000 | 376.295 → 0.784 | 445.577 → 68.435 | 530.753 → 158.869 |
| wide | 1,000 | 8.554 → 0.995 | 16.082 → 8.505 | 50.627 → 52.006 |
| deep | 512 | 174.193 → 0.330 | 177.945 → 3.950 | 207.240 → 54.453 |
| balanced | 1,023 | 29.672 → 1.608 | 37.197 → 9.162 | 71.822 → 49.523 |

The temporary adjacency index makes relationship discovery O(N). Usage reads each Session’s own Turns and composes already-computed child totals. Working keeps disjoint interval components in ordered sets, transferring the smaller child set into the larger; this retains closed intervals that ancestors can bridge into live work without copying every descendant history at every level. The bound is O(N + T log²(T + 1)) time and O(N + T) temporary space. No live relationship index needs maintenance.

The deterministic regression runs at `SessionStore::new`: 128 independent roots required 65,536 relationship checks before the change (test failed), and now require 384 indexed Session visits. Deep and wide fixtures bound actual Session/Turn visits and interval operations; a balanced-tree fixture compares every Session with the previous raw-Turn sorted-union reading. Existing live traversal is also counted so routing restoration through that scan fails the bound again. B-tree comparisons are logarithmic library operations, not counted individually.

Behavioral coverage includes root-only listing, child readings, absent versus zero Usage, interval gaps and touching/overlapping intervals, missing Turn timing, unchanged revisions/timestamps, no synthetic persistence writes, and orphan/cyclic relationship recovery. The latter preserves existing behavior: components without a readable root are not promoted into listed roots. The startup index is discarded, so existing live creation/deletion paths are unchanged.

Reproduce with an instrumented baseline and the new release binary:

```sh
cargo build --release --locked
python3 docs/benchmarks/session-restoration.py /absolute/path/to/baseline-suru
python3 docs/benchmarks/session-restoration.py target/release/suru
```

The harness emits all three samples and medians as JSON lines. Enable `SURU_LOG=suru::sessions=debug` for the same timing event outside the harness. The harness uses Python’s standard library and platform-absolute temporary paths. Execution and benchmarks were performed on Linux; Windows and macOS execution was not available locally.

Completed Linux checks:

- `cargo check --locked`: passed, repeated during implementation.
- `cargo nextest run --lib restoration_tests`: 6 passed.
- `cargo nextest run --test session_integration 'storage::' 'subagents::'`: 25 passed.
- `cargo nextest run --locked`: full suite, 1,676 passed and 4 skipped.
- `cargo fmt --all --check` and `git diff --check`: passed.

## Standards review

No findings. The diff follows documented provider-neutral, typed-domain, persistence, logging, and portable-path requirements. No actionable baseline smells were found.

## Spec review

No implementation or scope findings. One partial validation requirement remains: the issue asks to implement and test on Windows, macOS, and Linux; native Windows/macOS execution was unavailable. The temporary index, bottom-up Usage, durable interval union, recovery behavior, separate timing boundaries, and deterministic restoration checks meet the implementation requirements.

Standards: 0 findings. Spec: 1 validation gap (native Windows/macOS execution).

## Raw timing samples

Milliseconds, in run order for each case:

| Build / shape / Sessions | Projection | Store restoration | Readiness |
| --- | --- | --- | --- |
| Before / roots / 0 | 0.032, 0.002, 0.002 | 0.048, 0.018, 0.016 | 51.287, 27.088, 28.146 |
| Before / roots / 1,000 | 3.493, 3.349, 3.546 | 10.756, 10.509, 10.692 | 49.541, 49.031, 50.359 |
| Before / roots / 5,000 | 88.896, 90.794, 89.077 | 121.053, 122.940, 122.362 | 178.695, 184.903, 181.846 |
| Before / roots / 10,000 | 376.295, 380.490, 373.288 | 446.859, 445.577, 438.519 | 535.287, 524.484, 530.753 |
| Before / wide / 1,000 | 8.608, 8.441, 8.554 | 16.313, 16.073, 16.082 | 50.627, 50.464, 51.736 |
| Before / deep / 512 | 174.193, 179.747, 173.373 | 177.945, 183.558, 177.255 | 207.240, 211.319, 202.184 |
| Before / balanced / 1,023 | 29.539, 29.672, 29.741 | 37.084, 37.197, 37.231 | 71.822, 68.426, 74.020 |
| After / roots / 0 | 0.002, 0.002, 0.003 | 0.020, 0.017, 0.017 | 27.872, 28.941, 49.359 |
| After / roots / 1,000 | 0.041, 0.070, 0.044 | 6.965, 7.085, 7.117 | 54.066, 51.115, 50.198 |
| After / roots / 5,000 | 0.483, 0.458, 0.462 | 31.793, 32.360, 32.062 | 92.321, 92.590, 92.136 |
| After / roots / 10,000 | 0.784, 0.774, 0.811 | 68.435, 67.832, 69.795 | 155.483, 158.869, 162.457 |
| After / wide / 1,000 | 0.971, 1.019, 0.995 | 8.742, 8.505, 8.272 | 48.984, 52.357, 52.006 |
| After / deep / 512 | 0.240, 0.330, 0.394 | 3.902, 3.950, 4.298 | 54.772, 50.864, 54.453 |
| After / balanced / 1,023 | 1.608, 1.666, 1.559 | 8.750, 9.162, 9.164 | 49.523, 51.017, 48.699 |
