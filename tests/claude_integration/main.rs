#![cfg(unix)]

//! End-to-end tests for the Claude provider, driven by scripted stand-in binaries.
//!
//! Each module below covers one area of the integration; `support` holds the scripted CLI shared
//! between them.

use crate::support as server_support;
use crate::support::provider as provider_support;
use crate::support::scripted_binary as scripted_binary_support;

mod activity;
mod approvals;
mod availability;
mod broker;
mod compactions;
mod context_breakdown;
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
