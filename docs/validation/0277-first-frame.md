# First-frame validation for #277

Measured on Linux with warm filesystem caches, a 160-column × 40-row pseudo-terminal, isolated `SURU_STATE_DIR`, `SURU_DATA_DIR`, and `SURU_CONFIG_DIR`, and dedicated `SURU_CHANNEL=probe-277`. The nested `suru.json` configuration pinned `provider.{codex,copilot,claude}.enabled` to false. No production state or configuration was used.

Before: `0c9c6ba6c1c9b07fa7a6687c9787598534334404`. After: the #277 implementation in this commit. Both used `cargo build --locked` and `cargo build --release --locked`. The same temporary Python PTY harness and copied binaries measured both revisions. Each process exited through Ctrl+C after its first frame; each matrix stopped its isolated server on completion.

The harness timestamps process launch, observation of the final OSC 11 query, and observation of the first cursor-show sequence after that query. Terminal entry hides the cursor. For complete replies, the harness immediately sends all 16 palette entries, foreground `#222222`, and background `#ffffff` after the final query. Silent runs send no colors. Cursor-position requests are answered separately. Fresh means a newly started server, not a cold disk cache. There are 3 samples per fresh case and 5 per reused case; reused cases follow the final fresh launch without stopping its server. No other startup optimizations (#278–#282) were applied.

## Launch to first frame

Medians in milliseconds.

| Scenario | Release before | Release after | Debug before | Debug after |
| --- | ---: | ---: | ---: | ---: |
| Fresh server, no color reply | 189.91 | 77.81 | 432.95 | 315.48 |
| Reused server, no color reply | 129.25 | 25.55 | 255.21 | 152.91 |
| Reused server, immediate complete reply | 27.03 | 34.43 | 171.92 | 151.85 |

## Final color query to first frame

Medians in milliseconds, isolating the removed wait more closely.

| Scenario | Release before | Release after | Debug before | Debug after |
| --- | ---: | ---: | ---: | ---: |
| Fresh server, no color reply | 102.95 | 0.30 | 104.88 | 3.25 |
| Reused server, no color reply | 102.99 | 0.30 | 107.22 | 2.93 |
| Reused server, immediate complete reply | 0.75 | 0.52 | 3.67 | 3.45 |

Complete-reply launch time varies independently of the probe: the release median increased in this small sample while its query-to-frame interval stayed below a millisecond. These local samples establish removal of the serial color wait, not a universal launch-time guarantee. Timestamps observe PTY read chunks rather than individual writes.

## Appearance and input decisions

Startup issues the queries synchronously, then enters the normal event loop with unprobed terminal facts. The initial Settings snapshot still gates the first frame. System inherits terminal foreground and panel backgrounds until the actual background is known; palette entry 0 alone cannot establish the background. Late facts merge and trigger the existing repaint path. Named and user Themes honor explicit Light/Dark mode immediately; System mode uses the existing dark fallback until background luminance is known. A named Theme in System mode can therefore change variant on a late light reply. The separate two-second protocol parsing window is unchanged.

## Automated coverage

The silent-stream test holds the input sender open and issues queries synchronously, then renders a useful Application frame after Settings, without advancing any timer or supplying terminal input. Its red run against the former async probe stayed pending on the first poll. Stream coverage preserves keys and paste around color replies, discards malformed protocol replies, and exercises fragmented, partial, late, absent, and expired responses with millisecond parsing timeouts. Rendering tests cover inherited System panels, palette-before-background updates, late light repaint, and first-frame user Theme modes.

The Unix PTY responder withholds complete colors until after the first cursor-show sequence. The clean-exit test checks that late light colors repaint without reader input. Existing PTY tests verify restoration after startup protocol failure, normal exit, and manual server stop; Windows uses ConPTY for the existing lifecycle assertions and platform-neutral rendering tests for color behavior.

Windows and macOS execution was not available in this Linux session. No successful build or test execution on those operating systems is claimed. New code uses the shared input path; raw-escape PTY assertions remain `cfg(unix)`, and new path fixtures use temporary directories.

Final Linux checks: `cargo check --all-targets`, `cargo fmt --all -- --check`, debug/release locked builds, and `cargo nextest run --no-fail-fast` passed. The full suite passed all 1,660 tests with 4 skipped. Its first run identified six existing assertions that still expected black fallback panels; after updating those expectations, 199 affected rendering tests passed and the complete suite passed.

The code-review skill's separate Standards review reported zero findings. Spec review found no implementation correctness or scope issues, with Windows/macOS execution remaining the one partial validation requirement. It also suggested an optional stronger regression test around the entire startup function: the synchronous query test protects the query boundary, while the PTY test protects reply/repaint ordering and the measurement matrix supplies real-startup timing evidence.
