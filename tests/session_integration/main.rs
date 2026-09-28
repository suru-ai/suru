//! End-to-end session protocol tests running against a real server and provider.
//!
//! Each module below covers one area of the protocol; `support` holds the fixtures
//! shared between them.

#[path = "../support/mod.rs"]
#[allow(dead_code)]
mod server_support;

#[path = "../support/failing_provider.rs"]
mod failing_provider_support;

#[path = "../support/provider.rs"]
mod provider_support;

mod context_fill;
mod continuations;
mod hydration;
mod icons;
mod managed_client;
mod model_catalog;
mod monitoring;
mod multi_provider;
mod prompts;
mod provider_enablement;
mod selection;
mod settlement;
mod skills;
mod stops;
mod storage;
mod streams;
mod subagent_tree;
mod subagents;
mod support;
mod titles;
mod turns;
mod usage;
mod viewed;
mod watch_outcomes;
mod working;
mod workspace_icon_choice;
mod workspace_icons;

mod approval_posture;
mod approvals;
mod attachments;
mod broker;
mod questionnaires;
mod repositories;

mod worktree_branch_derivation;
mod worktree_navigation;

mod worktree_preparation;

mod worktree_recovery;
