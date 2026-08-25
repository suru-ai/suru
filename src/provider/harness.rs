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
mod shared;

pub(crate) use process::{
    HarnessLink, HarnessSpec, ProcessGuard, ProcessRegistry, ProcessStdio,
    run_harness_to_completion, spawn_harness_process, supervise_harness_process,
};
pub(crate) use shared::{HarnessConnector, HarnessInvalidator, SharedHarness, SharedHarnessHandle};
