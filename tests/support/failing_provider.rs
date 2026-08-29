use std::sync::Arc;

use suru::{
    protocol::{
        AgentSelection, ProviderId, SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus,
        SkillDescriptor, SkillId, SkillPromptDelivery, Workspace,
    },
    provider::{
        ProviderErrand, ProviderError, ProviderFuture, ProviderModelDiscovery, ProviderRuntime,
        ProviderSessionConnection, ProviderSessionRequest,
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

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
        Box::pin(async { Err(ProviderError::new("Model discovery is unavailable.")) })
    }

    fn skill_catalog(&self, workspace: &std::path::Path) -> ProviderFuture<'_, SkillCatalog> {
        let skills = [
            ("safe-review-id", "review"),
            ("safe-smaller-interface-id", "smaller-interface"),
        ]
        .into_iter()
        .map(|(id, name)| SkillDescriptor {
            id: SkillId::new(id),
            name: name.to_owned(),
            description: format!("Test Skill {name}"),
            scope: Some("Workspace".to_owned()),
        })
        .collect();
        let catalog = SkillCatalog {
            provider: self.provider_id(),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            skills,
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: vec![
                    SkillPromptDelivery::Initial,
                    SkillPromptDelivery::Queue,
                    SkillPromptDelivery::Steer,
                ],
            },
            status: SkillCatalogStatus::Fresh { warning: None },
        };
        Box::pin(async move { Ok(catalog) })
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

    fn errand_selection(&self) -> Option<AgentSelection> {
        None
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
