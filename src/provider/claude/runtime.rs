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
    catalog::model_descriptors,
    claude_error, claude_error_context,
    session::start_claude_session,
    transport::{ClaudeConnection, StreamJsonTransport},
    wire::{ControlRequest, NativeModelList},
};
use crate::{
    protocol::{ModelDescriptor, ProviderId},
    provider::{
        ProviderError, ProviderFuture, ProviderRuntime, ProviderSessionConnection,
        ProviderSessionRequest, harness::ProcessRegistry, resolve_executable,
    },
};

const CLAUDE_PATH_ENV: &str = "SURU_CLAUDE_PATH";
const CLAUDE_EXECUTABLE_NAME: &str = "claude";
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Launches one short-lived Claude Code CLI process per Model discovery.
#[derive(Clone, Debug)]
pub struct ClaudeRuntime {
    executable: OsString,
    processes: ProcessRegistry,
    control_request_timeout: Duration,
}

impl ClaudeRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        Self {
            executable: executable.as_ref().to_owned(),
            processes: ProcessRegistry::new(super::CLAUDE_HARNESS_NAME),
            control_request_timeout: CONTROL_REQUEST_TIMEOUT,
        }
    }

    /// Bounds how long a control request waits for the CLI to answer; injectable so tests with
    /// unresponsive fixtures do not wait out the default.
    pub fn with_control_request_timeout(mut self, timeout: Duration) -> Self {
        self.control_request_timeout = timeout;
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

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let request_timeout = self.control_request_timeout;
        Box::pin(
            async move { discover_claude_models(executable, processes, request_timeout).await },
        )
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        let executable = self.executable.clone();
        let processes = self.processes.clone();
        let request_timeout = self.control_request_timeout;
        Box::pin(async move {
            start_claude_session(executable, request, processes, request_timeout).await
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.processes.shutdown().await })
    }
}

pub(super) async fn discover_claude_models(
    executable: OsString,
    processes: ProcessRegistry,
    request_timeout: Duration,
) -> Result<Vec<ModelDescriptor>, ProviderError> {
    let ClaudeConnection { transport, process } =
        StreamJsonTransport::launch(&executable, std::iter::empty(), None, None, processes).await?;
    let result = transport
        .control_request(&ControlRequest::ListModels, request_timeout)
        .await
        .map_err(|error| claude_error_context("Claude Model discovery failed", error))?;
    let listed: NativeModelList = serde_json::from_value(result).map_err(|error| {
        claude_error(format!(
            "Claude Code CLI returned an invalid list_models response: {error}"
        ))
    })?;
    transport.close().await;
    process.wait_until_stopped().await?;
    Ok(model_descriptors(listed.models))
}
