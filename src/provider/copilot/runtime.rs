//! The Copilot Provider runtime and the one shared harness process behind it.

use std::ffi::{OsStr, OsString};

use serde_json::Value;
use tokio::time::Duration;

use super::{
    COPILOT_HARNESS_NAME, COPILOT_PROVIDER_ID, COPILOT_SERVER_ARGS, REASONING_EFFORT_OPTION_ID,
    catalog::model_descriptors,
    copilot_error_context,
    errand::run_copilot_errand,
    session::{INTERRUPT_REQUEST_TIMEOUT, start_copilot_session},
    transport::CopilotConnector,
};
use crate::{
    protocol::{
        AgentSelection, ModelDescriptor, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionSelection, ModelOptionValue, ProviderId,
    },
    provider::{
        ProviderErrand, ProviderFuture, ProviderRuntime, ProviderSessionConnection,
        ProviderSessionRequest,
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
    interrupt_request_timeout: Duration,
}

impl CopilotRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        let spec = HarnessSpec {
            executable: executable.as_ref().to_owned(),
            args: COPILOT_SERVER_ARGS.iter().map(OsString::from).collect(),
            name: COPILOT_HARNESS_NAME.to_owned(),
            cwd: None,
        };
        Self {
            harness: SharedHarness::new(spec, CopilotConnector::new()),
            interrupt_request_timeout: INTERRUPT_REQUEST_TIMEOUT,
        }
    }

    /// Bounds how long an interrupt waits for Copilot to acknowledge it; injectable so tests can
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

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
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
            Ok(model_descriptors(listed))
        })
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        Box::pin(async move {
            // Launches the shared process if this is the first demand, or the first since a crash,
            // which is how the Prompt after a harness crash recovers without a restart.
            let handle = self.harness.demand().await?;
            start_copilot_session(handle, request, self.interrupt_request_timeout).await
        })
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
