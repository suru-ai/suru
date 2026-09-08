#![cfg(unix)]

//! End-to-end tests for the Claude provider, driven by scripted stand-in binaries.
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

mod activity;
mod availability;
mod context_fill;
mod continuations;
mod errands;
mod interruption;
mod models;
mod resume;
mod skills;
mod steering;
mod subagents;
mod support;
mod turns;

mod questionnaires;
