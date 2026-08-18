use std::sync::Arc;

use chidori::{
    provider::{
        ProviderError, ProviderFuture, ProviderRuntime, ProviderSessionConnection,
        ProviderSessionRequest,
    },
    server::{self, ServerConfig},
};

pub struct FailingProviderRuntime;

impl ProviderRuntime for FailingProviderRuntime {
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
}

pub async fn spawn_with_failing_provider(
    config: ServerConfig,
) -> anyhow::Result<server::RunningServer> {
    server::spawn_with_provider(config, Arc::new(FailingProviderRuntime)).await
}
