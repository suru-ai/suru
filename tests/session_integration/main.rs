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

mod managed_client;
mod multi_provider;
mod prompts;
mod provider_enablement;
mod selection;
mod storage;
mod streams;
mod support;
mod titles;
mod turns;
