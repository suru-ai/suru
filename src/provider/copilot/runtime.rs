//! The Copilot Provider runtime and the one shared harness process behind it.

use std::{
    ffi::{OsStr, OsString},
    sync::{Arc, Mutex as StdMutex},
};

use serde_json::Value;
use tokio::{sync::watch, time::Duration};

use super::{
    COPILOT_HARNESS_NAME, COPILOT_PROVIDER_ID, COPILOT_SERVER_ARGS, REASONING_EFFORT_OPTION_ID,
    catalog::model_descriptors,
    copilot_error_context,
    errand::run_copilot_errand,
    session::{INTERRUPT_REQUEST_TIMEOUT, start_copilot_session},
    skills::CopilotSkills,
    transport::CopilotConnector,
};
use crate::{
    protocol::{
        AgentSelection, CopilotPermissions, EffectiveSettings, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionSelection, ModelOptionValue, ProviderId, ProviderUnavailability,
        SkillCatalog,
    },
    provider::{
        ManualCompaction, ProviderErrand, ProviderFuture, ProviderModelDiscovery, ProviderRuntime,
        ProviderSessionConnection, ProviderSessionRequest,
        harness::{HarnessSpec, SharedHarness},
        resolve_executable,
    },
};

const COPILOT_PATH_ENV: &str = "SURU_COPILOT_PATH";
const COPILOT_EXECUTABLE_NAME: &str = "copilot";

/// The Model Copilot declares its own Errands run at: the cheapest Model in the class Copilot's own
/// picker calls lightweight.
const ERRAND_MODEL_ID: &str = "gpt-5.6-luna";

/// The reasoning effort it declares them at, named in Copilot's own wire vocabulary: the one that
/// spends no thinking budget at all, because a Model asked to write six words should not be paid to
/// reason about them. Not every Copilot Model offers it, which is one more reason the Model and the
/// effort are declared together.
const ERRAND_REASONING_EFFORT: &str = "none";

/// Drives every Copilot Session and Model discovery through one supervised Copilot CLI server
/// process, launched on the first demand and relaunched fresh after a crash.
pub struct CopilotRuntime {
    harness: SharedHarness<CopilotConnector>,
    skills: CopilotSkills,
    interrupt_request_timeout: Duration,
    permissions: Arc<StdMutex<CopilotPermissions>>,
}

impl CopilotRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        let spec = HarnessSpec {
            executable: executable.as_ref().to_owned(),
            args: COPILOT_SERVER_ARGS.iter().map(OsString::from).collect(),
            name: COPILOT_HARNESS_NAME.to_owned(),
            cwd: None,
            env: Vec::new(),
        };
        Self {
            harness: SharedHarness::new(spec, CopilotConnector::new()),
            skills: CopilotSkills::default(),
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
            permissions: Arc::new(StdMutex::new(CopilotPermissions::default())),
        }
    }

    /// Bounds how long an interrupt waits for Copilot to acknowledge it — and with it the other
    /// requests Suru makes of a Session's loop on its own account: the task roster read that
    /// finds a detached shell's Watch, the cancel that stops one, and the context attribution a
    /// Context Breakdown reads. Injectable so tests can
    /// exercise the timeout without waiting out the default.
    pub fn with_interrupt_request_timeout(mut self, timeout: Duration) -> Self {
        self.interrupt_request_timeout = timeout;
        self
    }

    pub fn from_environment() -> Self {
        Self::new(resolve_executable(
            COPILOT_PATH_ENV,
            COPILOT_EXECUTABLE_NAME,
        ))
    }
}

impl Default for CopilotRuntime {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl ProviderRuntime for CopilotRuntime {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new(COPILOT_PROVIDER_ID)
    }

    fn display_name(&self) -> &str {
        "Copilot"
    }

    fn nerd_font_icon(&self) -> Option<char> {
        // Nerd Fonts `nf-cod-copilot` (Codicons Copilot).
        Some('\u{ec1e}')
    }

    // `session.history.compact` compacts a Session on request, keeping what its
    // `customInstructions` say the summary should.
    fn manual_compaction(&self) -> ManualCompaction {
        ManualCompaction::WithInstructions
    }

    fn offers_context_breakdown(&self) -> bool {
        true
    }

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
        Box::pin(async move {
            // Launches the shared process if this is the first demand, or the first since a crash.
            let handle = self.harness.demand().await?;
            // A process that dies mid-discovery is the answer, so it races the request rather than
            // leaving the demand waiting on a pipe nobody is left to write to.
            let listed = tokio::select! {
                biased;
                crashed = handle.crashed() => {
                    return Err(copilot_error_context("Copilot Model discovery failed", crashed));
                }
                listed = handle.connection().list_models() => listed?,
            };
            let discovery = ProviderModelDiscovery::new(model_descriptors(listed));
            Ok(match handle.connection().compatibility_warning() {
                Some(warning) => discovery.with_warning(warning),
                None => discovery,
            })
        })
    }

    fn skill_catalog(
        &self,
        execution_directory: &std::path::Path,
    ) -> ProviderFuture<'_, SkillCatalog> {
        let execution_directory = execution_directory.to_owned();
        Box::pin(async move {
            let handle = match self.harness.demand().await {
                Ok(handle) => handle,
                Err(error)
                    if error.unavailability()
                        == Some(ProviderUnavailability::IncompatibleVersion) =>
                {
                    return Ok(CopilotSkills::incompatible_catalog(&execution_directory));
                }
                Err(error) => return Err(error),
            };
            self.skills.discover(&handle, &execution_directory).await
        })
    }

    fn subscribe_skill_catalog_invalidations(&self) -> Option<watch::Receiver<u64>> {
        Some(self.skills.subscribe_invalidations())
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        Box::pin(async move {
            // Launches the shared process if this is the first demand, or the first since a crash,
            // which is how the Prompt after a harness crash recovers without a restart.
            let handle = self.harness.demand().await?;
            let permissions = *self
                .permissions
                .lock()
                .expect("Copilot permissions Setting lock is not poisoned");
            start_copilot_session(
                handle,
                request,
                self.skills.clone(),
                self.interrupt_request_timeout,
                permissions,
            )
            .await
        })
    }

    fn apply_settings(&self, settings: &EffectiveSettings) {
        *self
            .permissions
            .lock()
            .expect("Copilot permissions Setting lock is not poisoned") =
            settings.provider.copilot.permissions;
    }

    fn run_errand(&self, errand: ProviderErrand) -> ProviderFuture<'_, Value> {
        Box::pin(async move {
            // The same shared process every Session and discovery runs on, launched here too if
            // this is the first demand since the server started or since a crash.
            let handle = self.harness.demand().await?;
            run_copilot_errand(handle, errand).await
        })
    }

    /// The Agent Selection Copilot's own Errands run at.
    ///
    /// Copilot names no cheap Model of its own — its catalog marks a Model's price band and picker
    /// category but never says which one Copilot itself would use for a chore — so the choice is
    /// made here, and made as a whole Selection rather than a Model identifier, because Copilot
    /// relays reasoning efforts in its own publication order under its own wire names and nothing in
    /// that order says which is the least.
    ///
    /// Both halves are a declaration rather than a resolution. A Model withdrawn from the catalog,
    /// or one that stops offering this effort, gives way to Copilot's own default Model rather than
    /// failing the Errand.
    fn errand_selection(&self) -> Option<AgentSelection> {
        Some(AgentSelection {
            provider: ProviderId::new(COPILOT_PROVIDER_ID),
            model: ModelId::new(ERRAND_MODEL_ID),
            options: vec![ModelOptionSelection {
                id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(ERRAND_REASONING_EFFORT),
                },
            }],
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.harness.shutdown().await })
    }
}
