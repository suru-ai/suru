//! The Copilot Provider runtime and the one shared harness process behind it.

use std::ffi::{OsStr, OsString};

use tokio::time::Duration;

use super::{
    COPILOT_HARNESS_NAME, COPILOT_PROVIDER_ID, COPILOT_SERVER_ARGS,
    catalog::model_descriptors,
    copilot_error_context,
    session::{INTERRUPT_REQUEST_TIMEOUT, start_copilot_session},
    transport::CopilotConnector,
};
use crate::{
    protocol::{ModelDescriptor, ProviderId},
    provider::{
        ProviderFuture, ProviderRuntime, ProviderSessionConnection, ProviderSessionRequest,
        harness::{HarnessSpec, SharedHarness},
        resolve_executable,
    },
};

const COPILOT_PATH_ENV: &str = "SURU_COPILOT_PATH";
const COPILOT_EXECUTABLE_NAME: &str = "copilot";

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

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.harness.shutdown().await })
    }
}
