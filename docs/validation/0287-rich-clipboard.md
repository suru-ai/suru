# Rich Clipboard validation for #287

Automated validation uses headless rendered Application selections, recording native and terminal sinks, and controlled Clipboard workers. It covers paired Markdown/HTML, selection boundaries, formatting, escaping, both copy gestures, rich success without terminal output, native failure and terminal failure, millisecond fallback under stalls, superseded completions, latest-pending replacement, Linux primary selection, lazy handle ownership, and bounded shutdown. No test accesses a real Clipboard.

Manual paste validation could not be performed in the implementation environment:

| Environment | Formatted paste | Text-only paste | Terminal also writing Clipboard |
| --- | --- | --- | --- |
| Linux | Unavailable: terminal-only session with no DISPLAY or WAYLAND_DISPLAY | Unavailable: no desktop paste destination | Unavailable: no interactive Clipboard-capable terminal/paste destination |
| Windows | Unavailable: no Windows environment | Unavailable | Unavailable |
| macOS | Unavailable: no macOS environment | Unavailable | Unavailable |

When those environments are available, select Agent Message and visible Reasoning content containing nested lists, headings, emphasis, links, quotes, a pipe table, and mixed prose/code; repeat with partial selections and Code Block-only selections in both manual and copy-on-release modes. Paste into an HTML-capable editor and a text-only editor, checking that the selected content agrees and formatting survives. Repeat inside a terminal supporting Clipboard escape sequences to verify a successful native rich copy is not replaced by terminal text, and with native Clipboard access unavailable to verify Markdown terminal fallback. Plain-text Invite copies should still reach both destinations. Check Linux primary selection separately. Record the OS, terminal, paste application, and results before claiming manual validation.

Cross-target typechecking was attempted for the installed `x86_64-pc-windows-gnu` and `aarch64-apple-darwin` Rust targets. Both stopped in unchanged C dependencies (`ring` and `libsqlite3-sys`) before checking Suru: the Windows MinGW C compiler is absent, and the Linux C compiler cannot accept macOS target flags. These are not claimed as successful cross-platform builds.

Final Linux checks: `cargo check --all-targets` and `cargo fmt --all -- --check` passed. The complete `cargo nextest run --no-fail-fast` run passed all 1,657 tests with 4 skipped. An initial full run overlapping typechecking stopped on three provider-harness timing failures; the seven harness tests passed in isolation, and the complete rerun passed without code changes. The separate Standards and Spec reviews reported zero findings.
