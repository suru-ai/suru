//! Spawning a Server as this suite does: as [`suru::server`] spawns one, but
//! marked onboarded before any test attaches to it, as a Server whose user has
//! long since taken Onboarding is, so no test here meets the flow unasked. A
//! test of Onboarding itself takes its Server as it comes, never onboarded,
//! from [`spawn_not_onboarded`].
//!
//! Each launch is asked for as it is called, before it is polled, exactly as
//! the one it wraps is, so it follows the Channel's last Manual stop from
//! then; only once the Server has started is it marked.

pub use suru::server::*;

use std::sync::Arc;

use suru::{provider::ProviderRuntime, source_control::SourceControl};

/// The Server `launch` starts, marked onboarded once it has started.
fn onboarded(launch: impl ServerLaunch) -> impl ServerLaunch {
    async move {
        let server = launch.await?;
        crate::support::mark_onboarded(server.descriptor()).await;
        Ok(server)
    }
}

pub fn spawn(config: ServerConfig) -> impl ServerLaunch {
    onboarded(suru::server::spawn(config))
}

/// A Server as it comes, never onboarded, for a test of Onboarding itself.
pub fn spawn_not_onboarded(config: ServerConfig) -> impl ServerLaunch {
    suru::server::spawn(config)
}

pub fn spawn_with_provider(
    config: ServerConfig,
    runtime: Arc<dyn ProviderRuntime>,
) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_provider(config, runtime))
}

pub fn spawn_with_providers(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_providers(config, runtimes))
}

pub fn spawn_with_timings(config: ServerConfig, timings: ServerTimings) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_timings(config, timings))
}

pub fn spawn_with_provider_and_timings(
    config: ServerConfig,
    runtime: Arc<dyn ProviderRuntime>,
    timings: ServerTimings,
) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_provider_and_timings(
        config, runtime, timings,
    ))
}

pub fn spawn_with_providers_and_timings(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
    timings: ServerTimings,
) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_providers_and_timings(
        config, runtimes, timings,
    ))
}

pub fn spawn_with_source_control(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
    timings: ServerTimings,
    source_control: Arc<dyn SourceControl>,
) -> impl ServerLaunch {
    onboarded(suru::server::spawn_with_source_control(
        config,
        runtimes,
        timings,
        source_control,
    ))
}
