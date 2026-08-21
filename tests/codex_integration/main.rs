#![cfg(unix)]

//! End-to-end tests for the Codex provider, driven by scripted stand-in binaries.
//!
//! Each module below covers one area of the integration; `support` holds the scripts
//! and fixtures shared between them.

#[path = "../support/mod.rs"]
#[allow(dead_code)]
mod server_support;

mod errors;
mod interruption;
mod models;
mod process;
mod shutdown;
mod steering;
mod support;
mod turns;
