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
mod approvals;
mod availability;
mod broker;
mod context_fill;
mod continuations;
mod errands;
mod interruption;
mod models;
mod monitoring;
mod respawn;
mod resume;
mod skills;
mod steering;
mod subagent_steers;
mod subagents;
mod support;
mod turns;

mod questionnaires;

#[path = "../support/managed_worktree.rs"]
mod managed_worktree;
