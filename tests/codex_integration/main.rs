#![cfg(unix)]

//! End-to-end tests for the Codex provider, driven by scripted stand-in binaries.
//!
//! Each module below covers one area of the integration; `support` holds the scripts
//! and fixtures shared between them.

use crate::support as server_support;

mod activity;
mod approvals;
mod broker;
mod compactions;
mod context_breakdown;
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
