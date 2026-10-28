//! The Claude Provider runtime: short-lived stream-json spawns for Model discovery, and one
//! long-lived CLI process per Session.
//!
//! Like Codex, every demand launches its own supervised CLI process — there is no shared server
//! process to keep alive. A discovery's process lives for one control request; a Session's lives
//! from its first Turn until the Session shuts down (see [`super::session`]).

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    sync::{Arc, Mutex as StdMutex},
};

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
    wire::{ControlRequest, NativeModel, NativeModelList},
};
use crate::{
    pricing::{CacheRatesFuture, ModelsDevModel, PricingSource},
    protocol::{
        AgentSelection, ClaudePermissionMode, EffectiveSettings, ModelId, ProviderId, SkillCatalog,
    },
    provider::{
        ManualCompaction, ProviderErrand, ProviderError, ProviderFuture, ProviderModelDiscovery,
        ProviderRuntime, ProviderSessionConnection, ProviderSessionRequest,
        harness::ProcessRegistry, resolve_executable,
    },
};

const CLAUDE_PATH_ENV: &str = "SURU_CLAUDE_PATH";
/// The models.dev catalog Claude's canonical Models are priced in.
const MODELS_DEV_PROVIDER: &str = "anthropic";
/// The suffix a 1M-context alias or Model carries, which models.dev's Model IDs never do.
const ONE_MILLION_CONTEXT_SUFFIX: &str = "[1m]";
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
    posture_request_timeout: Duration,
    context_request_timeout: Duration,
    skills: ClaudeSkills,
    permission_mode: Arc<StdMutex<ClaudePermissionMode>>,
    /// The canonical Model each alias of the last listed catalog resolved to; absent until a
    /// catalog has been listed.
    resolved_models: Arc<StdMutex<Option<HashMap<String, String>>>>,
    /// The models.dev rate table a cache saving is estimated from. Claude reports its own Cost,
    /// so nothing else here reads it.
    pricing: Option<Arc<PricingSource>>,
}

impl ClaudeRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(super::CLAUDE_HARNESS_NAME),
            availability: ClaudeAvailability::new(),
            control_request_timeout: CONTROL_REQUEST_TIMEOUT,
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
            posture_request_timeout: CONTROL_REQUEST_TIMEOUT,
            context_request_timeout: Duration::from_secs(5),
            skills: ClaudeSkills::default(),
            permission_mode: Arc::new(StdMutex::new(ClaudePermissionMode::default())),
            resolved_models: Arc::default(),
            pricing: None,
        }
    }

    /// Hands this runtime the models.dev rate lookup a cache saving is estimated from;
    /// injectable so tests point it at a fixture rather than the network.
    pub fn with_pricing_source(mut self, pricing: Arc<PricingSource>) -> Self {
        self.pricing = Some(pricing);
        self
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

    /// Bounds how long a live permission-mode update waits for the CLI to acknowledge it;
    /// injectable so tests can watch one fail without shrinking the budget a Session's startup
    /// gets.
    pub fn with_posture_request_timeout(mut self, timeout: Duration) -> Self {
        self.posture_request_timeout = timeout;
        self
    }

    /// Bounds the entire optional context query, including sending it to a busy CLI.
    pub fn with_context_request_timeout(mut self, timeout: Duration) -> Self {
        self.context_request_timeout = timeout;
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

    fn nerd_font_icon(&self) -> Option<char> {
        // Nerd Fonts `nf-cod-claude` (Codicons Claude).
        Some('\u{ec82}')
    }

    // The CLI stops one task by id, which is exactly a per-Subagent stop.
    fn supports_subagent_stop(&self) -> bool {
        true
    }

    // The CLI compacts on its own `/compact`, which the Session runs as a Turn's loop, and reads
    // whatever follows the command as what the summary should keep.
    fn manual_compaction(&self) -> ManualCompaction {
        ManualCompaction::WithInstructions
    }

    fn offers_context_breakdown(&self) -> bool {
        true
    }

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let availability = self.availability.clone();
        let request_timeout = self.control_request_timeout;
        Box::pin(async move {
            let (discovery, resolved) =
                usable_claude_models(executable, processes, availability, request_timeout).await?;
            *self
                .resolved_models
                .lock()
                .expect("Claude resolved Models lock is not poisoned") = Some(resolved);
            Ok(discovery)
        })
    }

    /// The models.dev `anthropic` rates of the canonical Model `model` ran: an alias of the last
    /// listed catalog is read as the Model it resolved to then — today's, which may not be the
    /// one an older Turn ran — and any other ID as itself, a 1M-context suffix set aside. The
    /// cache-write rate models.dev lists is the five-minute one, and no long-context premium is
    /// priced. None until a catalog has been listed, since nothing is launched only to answer.
    fn cache_rates<'a>(&'a self, model: &'a ModelId) -> CacheRatesFuture<'a> {
        let canonical = self
            .resolved_models
            .lock()
            .expect("Claude resolved Models lock is not poisoned")
            .as_ref()
            .map(|resolved| {
                let model = model.as_str();
                let canonical = resolved.get(model).map_or(model, String::as_str);
                canonical
                    .strip_suffix(ONE_MILLION_CONTEXT_SUFFIX)
                    .unwrap_or(canonical)
                    .to_owned()
            });
        Box::pin(async move {
            self.pricing
                .as_ref()?
                .cache_rates(&ModelsDevModel::new(
                    MODELS_DEV_PROVIDER,
                    ModelId::new(canonical?),
                ))
                .await
        })
    }

    fn skill_catalog(
        &self,
        execution_directory: &std::path::Path,
    ) -> ProviderFuture<'_, SkillCatalog> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let skills = self.skills.clone();
        let execution_directory = execution_directory.to_owned();
        let request_timeout = self.control_request_timeout;
        Box::pin(async move {
            skills
                .discover(executable, processes, execution_directory, request_timeout)
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
            posture_request: self.posture_request_timeout,
            context_request: self.context_request_timeout,
        };
        let skills = self.skills.clone();
        let permission_mode = *self
            .permission_mode
            .lock()
            .expect("Claude permission mode Setting lock is not poisoned");
        Box::pin(async move {
            start_claude_session(
                executable,
                request,
                processes,
                availability,
                skills,
                timings,
                permission_mode,
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

    fn apply_settings(&self, settings: &EffectiveSettings) {
        *self
            .permission_mode
            .lock()
            .expect("Claude permission mode Setting lock is not poisoned") =
            settings.provider.claude.permission_mode;
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
) -> Result<(ProviderModelDiscovery, HashMap<String, String>), ProviderError> {
    let warning = availability
        .verify(&executable, &processes, request_timeout)
        .await?;
    let models = discover_claude_models(executable, processes, request_timeout).await?;
    let resolved = models
        .iter()
        .filter_map(|model| Some((model.value.clone(), model.resolved_model.clone()?)))
        .collect();
    let discovery = ProviderModelDiscovery::new(model_descriptors(models));
    Ok((
        match warning {
            Some(warning) => discovery.with_warning(warning),
            None => discovery,
        },
        resolved,
    ))
}

async fn discover_claude_models(
    executable: OsString,
    processes: ProcessRegistry,
    request_timeout: Duration,
) -> Result<Vec<NativeModel>, ProviderError> {
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
    Ok(listed.models)
}
