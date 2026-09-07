# Build identity validation for #278

## Identity contract

The executable carries a 32-byte allocated section: `.suru` in ELF/PE and `__DATA,__suru` in Mach-O. The record contains a format marker and a random UUID minted by the `suru-build-id` procedural macro **when the binary is compiled**. The macro does not enumerate build inputs. Cargo's executable compilation already depends on source, dependencies, features, compiler options and toolchain. A fresh compilation receives a fresh identity; an unchanged Cargo build or a copy of an existing executable retains its identity. Clean recompilation may therefore replace a server even when source is unchanged, and builds are no longer byte-reproducible.

The static is referenced by address through `black_box`, retaining its allocated storage through optimization, linker garbage collection and debug stripping. Readers inspect headers, section names and the 32-byte record, without loading symbol tables or executable payloads. Universal Mach-O identity includes every architecture's marker. The identity comes entirely from the configured executable, including custom paths, without mixing in the calling client's package version.

Each lookup opens the executable afresh. Atomic replacement at the same path is observed even when size and modification time are preserved; there is no metadata or persistent digest cache to invalidate. Missing, malformed, zero or unsupported markers fall back to full BLAKE3 hashing. Unmarked external executables consequently retain the expensive path. Packaging may strip debug information or sign a marked image while retaining its identity. Post-link patches that change behavior must regenerate the marker or remove it to select hashing: this is build compatibility metadata, not executable integrity verification. Concurrent in-place writes are not an atomic installation protocol; publishers should replace a completed executable atomically.

A native CodeView/PDB GUID was rejected during review: a relocation-only change can leave LLVM's PDB GUID unchanged. LLVM hashes input section data but [does not calculate relocation checksums for PDB section contributions](https://github.com/llvm/llvm-project/blob/main/lld/COFF/PDB.cpp). The dedicated compilation marker does not depend on that linker behavior.

## Direct identity cost

Linux, warm filesystem caches, five fresh processes per case. A small Rust harness compiled with `rustc -O` against the corresponding release Suru library times only the public `build_identity::for_executable` call. It consumes the identity without printing it; printing the elapsed duration happens after timing. Thus these are optimized reader/hash-code measurements over both executable profiles, not measurements of unoptimized hashing code.

The before executables and release library were the pre-change artifacts present in `target/`, copied before editing; that baseline was not freshly rebuilt from a pinned revision. Final artifacts were rebuilt with `cargo build --locked` and `cargo build --release --locked`. Before sizes: 429,006,392 bytes debug, 38,755,560 bytes release. Final sizes: 430,614,056 bytes debug, 38,775,592 bytes release.

| Executable profile | Before samples (ms) | Final samples (ms) | Before median | Final median |
| --- | --- | --- | ---: | ---: |
| release | 9.5271, 17.9037, 9.5566, 9.5509, 13.6301 | 0.0326, 0.0275, 0.0285, 0.0222, 0.0281 | 9.5566 ms | 0.0281 ms |
| debug | 123.1265, 123.1555, 98.5584, 97.0116, 97.9058 | 0.0295, 0.0283, 0.0253, 0.0239, 0.0338 | 98.5584 ms | 0.0283 ms |

A separate Linux `/proc/self/io` harness measured bytes returned by reads around the same public call, subtracting the counter-read overhead: **5,851 bytes release, 6,832 bytes debug**. This confirms the routine path avoids a full executable scan, independently of timing noise.

## Launch to first frame

A temporary Python PTY harness used a 160-column × 40-row terminal, isolated state/data/config roots, channel `probe-278`, and all three Providers disabled through a nested `suru.json`. It answered cursor-position requests and immediately sent complete palette/foreground/background replies after the final OSC 11 query. It timestamps process launch to the first cursor-show sequence after that query, then exits the client with Ctrl+C. One unrecorded warmup precedes each profile. Fresh means a new server process, not a cold disk cache. Three fresh and five reused-server samples were collected; isolated servers were stopped on completion. No production configuration or server was changed.

Medians in milliseconds:

| Profile | Server state | Before | Final |
| --- | --- | ---: | ---: |
| release | fresh | 78.24 | 76.35 |
| release | reused | 26.12 | 16.25 |
| debug | fresh | 361.38 | 82.26 |
| debug | reused | 154.56 | 25.09 |

Final raw samples (ms):

- release, fresh: 76.70, 74.88, 76.35.
- release, reused: 16.12, 20.02, 15.71, 16.44, 16.25.
- debug, fresh: 82.08, 86.13, 82.26.
- debug, reused: 25.09, 25.01, 24.40, 26.83, 25.55.

These small local samples establish removal of full-file hashing, not a universal launch-time guarantee. Release fresh-server time remains dominated by other startup work, including the unchanged readiness polling. Reuse and fresh startup are reported separately because the latter includes server creation as well as client launch.

## Validation

Regression coverage uses the public file identity and launcher seams. Synthetic ELF, Mach-O, PE and universal fixtures cover packaging invariance, every architecture, same-size/timestamp rebuilds, malformed/truncated/zero records and hashing fallback. The real packaged Suru binary must expose a marker in the tested profile. A temporary Cargo project using the real macro checks unchanged builds, relocation-only main-source changes, another module, a dependency, feature changes, optimization changes and stripped builds. It completes in about two seconds without production-scale sleeps. CLI coverage verifies reuse, atomic same-path replacement of a configured executable, subsequent reuse, channel isolation, protocol mismatches and concurrent launch convergence.

The format reader independently typechecked for `aarch64-apple-darwin` and `x86_64-pc-windows-gnu` in a temporary minimal crate; that check used BLAKE3's portable `pure` feature to avoid requiring foreign assembly toolchains. Additional real Rust cross-codegen fixtures used the actual procedural macro, `opt-level=3` and address retention, then linked with `ld64.lld -dead_strip` for macOS and `lld-link /DEBUG /OPT:REF /INCREMENTAL:NO` for PE. The final public reader recovered their markers in 840 and 656 bytes respectively. These exercises verify real foreign executable formats and linker retention, but **native Windows/macOS application execution remains unverified** in this Linux environment.

Linux locked debug/release builds, `cargo check --all-targets`, formatting and all 40 focused identity/CLI tests passed. An earlier broader run hit an existing pairing listener-bind failure; its rerun passed all 84 tests. The final `cargo nextest run --no-fail-fast` passed all **1,667 tests**, with 4 skipped, in 34.855 seconds.

The final code-review skill reviews found zero Standards findings and zero Spec implementation findings. The Spec review retained the native Windows/macOS validation gap.
