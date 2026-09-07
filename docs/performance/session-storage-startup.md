# Session history hydration benchmark (#280)

Measured on Linux on 2026-09-07 using release builds, warm filesystem caches, isolated temporary state/data roots, built-in Settings, and an unavailable fixture Provider. No real Provider process or user configuration is used. Each fixture is prepared in a parent process; a fresh child process measures its first server initialization. Values below are medians of three independent measured processes per case. The raw measurements are in [session-storage-startup.csv](session-storage-startup.csv).

The eager baseline is commit `9b20770bad7b35843f998144d05fa309a2c42962` with only the benchmark and storage tracing instrumentation added in a detached temporary checkout. Initial in-process measurements established the content-size cost before implementation; these replacement measurements use a fresh process to remove the fixture server’s allocator/setup effects.

Each root Session has the specified number of completed, promptless Turns. Every Turn has one completed agent Message and one Reasoning Activity, each containing the specified number of bytes. Session count, Turn count, and content size vary independently. This synthetic fixture isolates stored history costs; it does not model every user history or provider startup.

| Sessions | Turns/Session | KiB/content | Startup ms, eager → lazy | Queries, eager → lazy | Decode ms, eager → lazy | First open ms, eager → lazy |
|---:|---:|---:|---:|---:|---:|---:|
| 10 | 10 | 4 | 19.23 → 17.18 | 59 → 10 | 0.48 → 0.08 | 7.53 → 8.02 |
| 10 | 100 | 4 | 31.78 → 13.21 | 59 → 10 | 5.05 → 0.42 | 10.02 → 10.13 |
| 100 | 10 | 4 | 27.87 → 14.59 | 509 → 10 | 2.75 → 0.60 | 6.08 → 7.87 |
| 100 | 100 | 4 | 110.70 → 23.53 | 509 → 10 | 34.48 → 4.11 | 8.48 → 9.54 |
| 100 | 100 | 16 | 382.18 → 23.89 | 509 → 10 | 160.48 → 3.90 | 15.10 → 19.20 |
| 1000 | 10 | 4 | 127.89 → 32.23 | 5009 → 10 | 31.16 → 5.43 | 6.34 → 7.82 |

| Sessions | Turns/Session | KiB/content | Sampled startup peak MiB, eager → lazy | Steady MiB before first open, eager → lazy |
|---:|---:|---:|---:|---:|
| 10 | 10 | 4 | 20.81 → 19.38 | 20.81 → 19.38 |
| 10 | 100 | 4 | 38.00 → 20.11 | 38.00 → 20.11 |
| 100 | 10 | 4 | 39.05 → 22.08 | 39.05 → 22.08 |
| 100 | 100 | 4 | 191.98 → 28.61 | 191.98 → 28.61 |
| 100 | 100 | 16 | 663.23 → 28.40 | 663.23 → 28.40 |
| 1000 | 10 | 4 | 211.48 → 48.24 | 211.48 → 48.24 |

For 100 Sessions × 100 Turns × 16 KiB, readiness fell from 382.18 ms to 23.89 ms (about 94%), while pre-open steady memory fell from 663.23 MiB to 28.40 MiB (about 96%). First open increased from 15.10 ms to 19.20 ms. Smaller fixtures are dominated by fixed setup and scheduling noise, so these ratios are not a general latency forecast. The deliberate tradeoff is that first access now reads and decodes the containing Session tree; a large subtree can cost more than these root-only first opens.

Startup time covers the server spawn/setup call through readiness, excluding process launch and fixture creation. First-open latency covers HTTP client construction, the read, serialization, and decoding the response. Repository query counts are Diesel StartQuery events including connection PRAGMA batches, migration/schema checks, and saved Landing selection reads. Content loading changes from one Session query plus five queries per Session to one Session query plus one batched Turn query; both have eight fixed repository query events here.

Decode time sums repository decode/projection timers, excluding database reads and subsequent SessionStore/writer copies. The eager timer covers history payload decoding and Transcript assembly; the lazy startup timer covers Session and Turn metadata decoding. It is a stage diagnostic rather than complete CPU accounting. Memory is process resident memory sampled with sysinfo approximately every 2 ms, also sampled at readiness. Peak includes the readiness reading and can miss shorter transients; it is not a platform allocation profiler. The median sampled peak and steady readings coincide in these cases. Pre-start process RSS is retained in the CSV. The executable was measured on Linux only; the harness uses cross-platform paths/process and memory interfaces, but no Windows/macOS performance claims are made.

Reproduce a case with:

```sh
cargo run --release --locked --example storage_startup_perf -- 100 100 16384
```

The large fixtures are confined to this explicitly invoked example. Ordinary tests should use small histories. Startup still scales with Turn count because Working and subtree Usage are recovered from durable Turn metadata. Unopened Prompts, Messages, Activities, and Resume State never enter the canonical store or writer; clean hydration is not a storage write.
