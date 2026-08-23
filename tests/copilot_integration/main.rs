#![cfg(unix)]

//! End-to-end tests for the Copilot provider, driven by scripted stand-in binaries.
//!
//! Each module below covers one area of the integration; `support` holds the scripted CLI shared
//! between them.

#[path = "../support/mod.rs"]
#[allow(dead_code)]
mod server_support;

#[path = "../support/scripted_binary.rs"]
#[allow(dead_code)]
mod scripted_binary_support;

#[path = "../support/provider.rs"]
#[allow(dead_code)]
mod provider_support;

mod models;
mod support;
