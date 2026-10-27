//! Suru's integration tests, built as one binary.
//!
//! Every test binary links the whole of Suru, so each suite built as a binary
//! of its own was another link of it after every change to the library. Each
//! suite is instead the module its binary's crate root once was: a test's path
//! still begins with its suite's name, so `cargo nextest run -E
//! 'test(/^tui_rendering::/)'` runs one suite, and a suite reaches its own
//! modules through `crate::<suite>`. nextest still runs every test in a
//! process of its own.
//!
//! `relay_system` stays a binary of its own: it sets the environment and the
//! Log for its whole process.

#[path = "support/build_id.rs"]
mod build_id;

// The fixtures the suites share, built once for all of them. Each suite
// imports what it uses under the name it gave its own copy.
#[allow(dead_code)]
mod support;

#[path = "broker_smoke.rs"]
mod broker_smoke;

#[path = "build_identity.rs"]
mod build_identity;

#[path = "claude_integration/main.rs"]
mod claude_integration;

#[path = "cli_integration.rs"]
mod cli_integration;

#[path = "codex_integration/main.rs"]
mod codex_integration;

#[path = "copilot_integration/main.rs"]
mod copilot_integration;

#[path = "server_integration.rs"]
mod server_integration;

#[path = "session_integration/main.rs"]
mod session_integration;

#[path = "session_protocol.rs"]
mod session_protocol;

#[path = "settings_integration.rs"]
mod settings_integration;

#[path = "source_control.rs"]
mod source_control;

#[path = "transcript_perf.rs"]
mod transcript_perf;

#[path = "tui_rendering/main.rs"]
mod tui_rendering;
