#![cfg(unix)]

//! End-to-end tests for the Codex provider, driven by scripted stand-in binaries.
//!
//! Each module below covers one area of the integration; `support` holds the scripts
//! and fixtures shared between them.

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
mod approvals;
mod broker;
mod errands;
mod errors;
mod interruption;
mod models;
mod process;
mod questionnaires;
mod shutdown;
mod skills;
mod steering;
mod subagents;
mod support;
mod turns;
mod usage;

#[path = "../support/managed_worktree.rs"]
mod managed_worktree;
