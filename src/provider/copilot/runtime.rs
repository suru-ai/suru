//! The Copilot Provider runtime and the one shared harness process behind it.

use std::ffi::{OsStr, OsString};

use tokio::time::Duration;

use super::{
    COPILOT_HARNESS_NAME, COPILOT_SERVER_ARGS, catalog::model_descriptors, copilot_error,
    copilot_error_context, transport::CopilotConnector,
};
use crate::{
    protocol::{ModelDescriptor, ProviderId},
    provider::{
        ProviderFuture, ProviderRuntime, ProviderSessionConnection, ProviderSessionRequest,
        harness::{HarnessSpec, SharedHarness},
    },
};

const COPILOT_PATH_ENV: &str = "SURU_COPILOT_PATH";

/// Drives every Copilot Session and Model discovery through one supervised Copilot CLI server
/// process, launched on the first demand and relaunched fresh after a crash.
pub struct CopilotRuntime {
    harness: SharedHarness<CopilotConnector>,
}

impl CopilotRuntime {
    pub fn new(executable: impl AsRef<OsStr>) -> Self {
        let spec = HarnessSpec {
            executable: executable.as_ref().to_owned(),
            args: COPILOT_SERVER_ARGS.iter().map(OsString::from).collect(),
            name: COPILOT_HARNESS_NAME.to_owned(),
        };
        Self {
            harness: SharedHarness::new(spec, CopilotConnector::new()),
        }
    }

    pub fn from_environment() -> Self {
        Self::new(resolve_executable())
    }

    /// Bounds how long a stopping Copilot process may exit gracefully before it is forced down;
    /// injectable so tests with fixtures that ignore stdin closure do not wait out the default.
    pub fn with_process_exit_grace(mut self, exit_grace: Duration) -> Self {
        self.harness = self.harness.with_process_exit_grace(exit_grace);
        self
    }
}

/// Resolves the Copilot CLI the way Suru resolves every Provider's: an explicit override first,
/// then the binary's own name for the PATH to answer. Suru never installs or updates it.
fn resolve_executable() -> OsString {
    std::env::var_os(COPILOT_PATH_ENV)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| OsString::from("copilot"))
}

impl Default for CopilotRuntime {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl ProviderRuntime for CopilotRuntime {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new("copilot")
    }

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        Box::pin(async move {
            // Launches the shared process if this is the first demand, or the first since a crash.
            let handle = self.harness.demand().await?;
            let connection = handle.connection();
            // The SDK caches the Models it listed for the life of its client, and a catalog refresh
            // is a request for what Copilot offers *now*, so this asks the CLI directly.
            let rpc = connection.client().rpc();
            let models = rpc.models();
            let listed = tokio::select! {
                biased;
                crashed = handle.crashed() => {
                    return Err(copilot_error_context("Copilot Model discovery failed", crashed));
                }
                listed = models.list() => listed,
            };
            let listed = listed
                .map_err(|error| connection.failure("Copilot Model discovery failed", error))?;
            Ok(model_descriptors(listed.models))
        })
    }

    fn start_session(
        &self,
        _request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        // Copilot's Model catalog lands one ticket ahead of its Sessions (#118): the Provider is
        // selectable in the picker before a Turn can run against it.
        Box::pin(async move { Err(copilot_error("Copilot cannot open a Session yet")) })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.harness.shutdown().await })
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use super::{COPILOT_PATH_ENV, resolve_executable};

    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_uses_the_override_or_copilot_from_path() {
        let _environment = ENVIRONMENT
            .lock()
            .expect("Copilot environment test lock is not poisoned");
        let original = std::env::var_os(COPILOT_PATH_ENV);

        // SAFETY: this unit test serializes every mutation of this process variable and restores it
        // before releasing the lock. No production task is running in the unit-test process.
        unsafe {
            std::env::set_var(COPILOT_PATH_ENV, "/fixture/custom-copilot");
        }
        assert_eq!(
            resolve_executable(),
            OsString::from("/fixture/custom-copilot")
        );

        // SAFETY: covered by the serialized test scope described above.
        unsafe {
            std::env::set_var(COPILOT_PATH_ENV, "");
        }
        assert_eq!(
            resolve_executable(),
            OsString::from("copilot"),
            "an empty override is no override"
        );

        // SAFETY: covered by the serialized test scope described above.
        unsafe {
            std::env::remove_var(COPILOT_PATH_ENV);
        }
        assert_eq!(resolve_executable(), OsString::from("copilot"));

        // SAFETY: restore the exact environment observed before the serialized test scope.
        unsafe {
            if let Some(original) = original {
                std::env::set_var(COPILOT_PATH_ENV, original);
            } else {
                std::env::remove_var(COPILOT_PATH_ENV);
            }
        }
    }
}
