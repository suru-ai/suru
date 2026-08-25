//! The Claude Provider runtime: short-lived stream-json spawns for Model discovery, and one
//! long-lived CLI process per Session.
//!
//! Like Codex, every demand launches its own supervised CLI process — there is no shared server
//! process to keep alive. A discovery's process lives for one control request; a Session's lives
//! from its first Turn until the Session shuts down (see [`super::session`]).

use std::ffi::{OsStr, OsString};

use tokio::time::Duration;

use super::{
    CLAUDE_PROVIDER_ID,
    availability::ClaudeAvailability,
    catalog::model_descriptors,
    claude_error, claude_error_context,
    errand::run_claude_errand,
    session::{ClaudeTimings, start_claude_session},
    skills::ClaudeSkills,
    transport::{ClaudeConnection, ClaudeSettingSources, StreamJsonTransport},
    wire::{ControlRequest, NativeModelList},
};
use crate::{
    protocol::{AgentSelection, ModelDescriptor, ProviderId, SkillCatalog},
    provider::{
        ProviderErrand, ProviderError, ProviderFuture, ProviderRuntime, ProviderSessionConnection,
        ProviderSessionRequest, harness::ProcessRegistry, resolve_executable,
    },
};

const CLAUDE_PATH_ENV: &str = "SURU_CLAUDE_PATH";
const CLAUDE_EXECUTABLE_NAME: &str = "claude";
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A user stopping a Turn is waiting on the answer, so an interrupt — and each of the task stops
/// that go ahead of it — gives the CLI far less time than an ordinary control request gets.
const INTERRUPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Launches one short-lived Claude Code CLI process per availability probe and per Model discovery.
#[derive(Clone, Debug)]
pub struct ClaudeRuntime {
    executable: OsString,
    processes: ProcessRegistry,
    availability: ClaudeAvailability,
    control_request_timeout: Duration,
    interrupt_request_timeout: Duration,
    skills: ClaudeSkills,
}

impl ClaudeRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(super::CLAUDE_HARNESS_NAME),
            availability: ClaudeAvailability::new(),
            control_request_timeout: CONTROL_REQUEST_TIMEOUT,
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
            skills: ClaudeSkills::default(),
        }
    }

    /// Bounds how long a verdict that Claude is usable stands before the CLI is probed again;
    /// injectable so tests can watch one expire without waiting out the default.
    pub fn with_availability_ttl(mut self, ttl: Duration) -> Self {
        self.availability.set_ttl(ttl);
        self
    }

    /// Bounds how long a control request waits for the CLI to answer; injectable so tests with
    /// unresponsive fixtures do not wait out the default.
    pub fn with_control_request_timeout(mut self, timeout: Duration) -> Self {
        self.control_request_timeout = timeout;
        self
    }

    /// Bounds how long an interrupt waits for the CLI to acknowledge it, and how long each of the
    /// task stops that precede it does; injectable so tests with fixtures that never answer do not
    /// wait out the default.
    pub fn with_interrupt_request_timeout(mut self, timeout: Duration) -> Self {
        self.interrupt_request_timeout = timeout;
        self
    }

    /// Bounds how long a stopping CLI process may exit gracefully before it is forced down;
    /// injectable so tests with fixtures that ignore stdin closure do not wait out the default.
    pub fn with_process_exit_grace(mut self, exit_grace: Duration) -> Self {
        self.processes.set_exit_grace(exit_grace);
        self
    }

    pub fn from_environment() -> Self {
        Self::new(resolve_executable(CLAUDE_PATH_ENV, CLAUDE_EXECUTABLE_NAME))
    }
}

impl Default for ClaudeRuntime {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl ProviderRuntime for ClaudeRuntime {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(CLAUDE_PROVIDER_ID)
    }

    // "Claude", deliberately not "Claude Code": Suru hosts the Provider, and
    // the product the user is choosing between here is the model family.
    fn display_name(&self) -> &str {
        "Claude"
    }

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let availability = self.availability.clone();
        let request_timeout = self.control_request_timeout;
        Box::pin(async move {
            usable_claude_models(executable, processes, availability, request_timeout).await
        })
    }

    fn skill_catalog(&self, workspace: &std::path::Path) -> ProviderFuture<'_, SkillCatalog> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let skills = self.skills.clone();
        let workspace = workspace.to_owned();
        let request_timeout = self.control_request_timeout;
        Box::pin(async move {
            skills
                .discover(executable, processes, workspace, request_timeout)
                .await
        })
    }

    fn skill_delivery_rejection_guidance(
        &self,
        delivery: crate::protocol::SkillPromptDelivery,
    ) -> Option<&'static str> {
        (delivery == crate::protocol::SkillPromptDelivery::Steer)
            .then_some("queue this Prompt instead")
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let availability = self.availability.clone();
        let timings = ClaudeTimings {
            control_request: self.control_request_timeout,
            interrupt_request: self.interrupt_request_timeout,
        };
        let skills = self.skills.clone();
        Box::pin(async move {
            start_claude_session(
                executable,
                request,
                processes,
                availability,
                skills,
                timings,
            )
            .await
        })
    }

    // Claude fulfils Errands through the CLI's own print mode, which takes the
    // Errand's schema natively — so Claude never falls back to starting a
    // Provider-side session and discarding it (ADR 0011). The launch runs no
    // availability probe of its own: an Errand is best-effort, its Selection
    // was already resolved against a catalog a probe stands behind, and a CLI
    // that has become unusable since says so in the result it prints.
    fn run_errand(&self, errand: ProviderErrand) -> ProviderFuture<'_, serde_json::Value> {
        let executable = self.executable.clone();
        Box::pin(async move { run_claude_errand(executable, errand).await })
    }

    // Claude's own Errands run at the cheapest, fastest row its picker offers,
    // which is a different question from the Model a user converses with.
    fn errand_selection(&self) -> Option<AgentSelection> {
        Some(super::catalog::errand_selection())
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.processes.shutdown().await })
    }
}

/// The Models a Claude that can be used at all offers.
///
/// The probe goes first, and its condition is the answer whenever there is one: a Provider the user
/// has to install, sign in to, or update is one whose Model catalog would fail anyway, and saying
/// which condition it is, is what a client needs to tell the user what to do about it.
pub(super) async fn usable_claude_models(
    executable: OsString,
    processes: ProcessRegistry,
    availability: ClaudeAvailability,
    request_timeout: Duration,
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    availability
        .verify(&executable, &processes, request_timeout)
        .await?;
    discover_claude_models(executable, processes, request_timeout).await
}

async fn discover_claude_models(
    executable: OsString,
    processes: ProcessRegistry,
    request_timeout: Duration,
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let ClaudeConnection { transport, process } = StreamJsonTransport::launch(
        &executable,
        std::iter::empty(),
        None,
        None,
        processes,
        ClaudeSettingSources::Isolated,
    )
    .await?;
    let result = transport
        .control_request(&ControlRequest::ListModels, request_timeout)
        .await
        .map_err(|failure| {
            claude_error_context("Claude Model discovery failed", failure.into_error())
        })?;
    let listed: NativeModelList = serde_json::from_value(result).map_err(|error| {
        claude_error(format!(
            "Claude Code CLI returned an invalid list_models response: {error}"
        ))
    })?;
    transport.close().await;
    process.wait_until_stopped().await?;
    Ok(model_descriptors(listed.models))
}
