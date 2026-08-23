//! Provider-neutral machinery for the harness server processes Suru launches.
//!
//! A harness is the Provider-owned executable Suru drives a Provider through — the Codex
//! app-server, the Copilot CLI in server mode. [`process`] owns launching and supervising those
//! child processes with one grace-then-kill discipline, and [`shared`] builds the shared-process
//! topology on top of it: one lazily launched process per Provider runtime, hosting all of that
//! Provider's Sessions, relaunched fresh on the first demand after a crash.
//!
//! Both harness shapes stay expressible behind the Provider runtime seam: a runtime may launch one
//! supervised process per Session directly from [`process`] (as Codex does today), or own a single
//! [`shared::SharedHarness`] every Session and Model discovery demand goes through.

mod process;
// The shared cell's first Provider consumer is the Copilot runtime (#110),
// with Codex's migration tracked as #111; until one lands, only the cell's
// own tests exercise it.
#[allow(dead_code)]
mod shared;

pub(crate) use process::{
    HarnessLink, HarnessSpec, ProcessGuard, ProcessRegistry, ProcessStdio, spawn_harness_process,
    supervise_harness_process,
};
#[allow(unused_imports)]
pub(crate) use shared::{HarnessConnector, SharedHarness, SharedHarnessHandle};
