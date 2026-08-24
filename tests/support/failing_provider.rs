use std::sync::Arc;

use suru::{
    protocol::{ModelDescriptor, ProviderId},
    provider::{
        ProviderErrand, ProviderError, ProviderFuture, ProviderRuntime, ProviderSessionConnection,
        ProviderSessionRequest,
    },
    server::{self, ServerConfig},
};

pub struct FailingProviderRuntime;

impl ProviderRuntime for FailingProviderRuntime {
    fn provider_id(&self) -> ProviderId {
        ProviderId::new("failing")
    }

    fn display_name(&self) -> &str {
        "failing"
    }

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        Box::pin(async { Err(ProviderError::new("Model discovery is unavailable.")) })
    }

    fn start_session(
        &self,
        _request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        Box::pin(async {
            Err(ProviderError::new(
                "No Provider runtime is configured for this test server.",
            ))
        })
    }

    fn run_errand(&self, _errand: ProviderErrand) -> ProviderFuture<'_, serde_json::Value> {
        Box::pin(async {
            Err(ProviderError::new(
                "No Provider runtime is configured for this test server.",
            ))
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub async fn spawn_with_failing_provider(
    config: ServerConfig,
) -> anyhow::Result<server::RunningServer> {
    server::spawn_with_provider(config, Arc::new(FailingProviderRuntime)).await
}
