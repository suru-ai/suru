//! Server-owned Skill Catalog lookup and Prompt binding admission.

use std::{collections::HashSet, fs, path::Path, sync::Arc};

use tokio::sync::watch;

use crate::{
    protocol::{
        InitialPrompt, ProviderId, SettingsSnapshot, SkillCatalog, SkillCatalogCapabilities,
        SkillCatalogRequest, SkillCatalogStatus, SkillId, SkillPromptDelivery, Workspace,
    },
    provider::ProviderRuntime,
};

#[derive(Clone)]
pub(crate) struct SkillCatalogService {
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    settings: watch::Receiver<SettingsSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SkillCatalogError {
    InvalidWorkspace,
    ProviderNotHosted(ProviderId),
    Discovery,
    InvalidCatalog(String),
    InvalidInvocation(String),
}

impl SkillCatalogService {
    pub(crate) fn new(
        runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        Self { runtimes, settings }
    }

    pub(crate) async fn list(
        &self,
        request: SkillCatalogRequest,
    ) -> Result<SkillCatalog, SkillCatalogError> {
        let workspace = canonical_workspace(&request.workspace.path)?;
        let runtime = self
            .runtimes
            .iter()
            .find(|runtime| runtime.provider_id() == request.provider)
            .ok_or_else(|| SkillCatalogError::ProviderNotHosted(request.provider.clone()))?;
        if !self
            .settings
            .borrow()
            .settings
            .provider_enabled(&request.provider)
        {
            return Ok(SkillCatalog {
                provider: request.provider,
                workspace: Workspace { path: workspace },
                skills: Vec::new(),
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: Some(0),
                    supported_deliveries: Vec::new(),
                },
                status: SkillCatalogStatus::Unavailable {
                    message: "Provider is disabled".to_owned(),
                },
            });
        }
        let catalog = runtime.skill_catalog(&workspace).await.map_err(|error| {
            tracing::warn!(provider = %request.provider, "Skill discovery failed: {error}");
            SkillCatalogError::Discovery
        })?;
        validate_catalog(&catalog, &request.provider, &workspace)?;
        Ok(catalog)
    }

    pub(crate) async fn validate_prompt(
        &self,
        provider: ProviderId,
        workspace: &Path,
        prompt: &InitialPrompt,
        delivery: SkillPromptDelivery,
    ) -> Result<(), SkillCatalogError> {
        if prompt.skill_invocations.is_empty() {
            return Ok(());
        }
        let catalog = self
            .list(SkillCatalogRequest {
                provider,
                workspace: Workspace {
                    path: workspace.to_owned(),
                },
            })
            .await?;
        if !matches!(catalog.status, SkillCatalogStatus::Fresh { .. }) {
            return Err(SkillCatalogError::InvalidInvocation(
                "the Provider's Skill Catalog is not current".to_owned(),
            ));
        }
        if !catalog
            .capabilities
            .supported_deliveries
            .contains(&delivery)
        {
            return Err(SkillCatalogError::InvalidInvocation(format!(
                "the Provider does not support Skill Invocations for {delivery:?} Prompts"
            )));
        }

        let distinct = prompt
            .skill_invocations
            .iter()
            .map(|invocation| &invocation.skill_id)
            .collect::<HashSet<_>>();
        if catalog
            .capabilities
            .max_distinct_invocations
            .is_some_and(|limit| distinct.len() > limit as usize)
        {
            return Err(SkillCatalogError::InvalidInvocation(
                "the Prompt invokes more distinct Skills than the Provider supports".to_owned(),
            ));
        }

        for invocation in &prompt.skill_invocations {
            let descriptor = catalog
                .skills
                .iter()
                .find(|skill| skill.id == invocation.skill_id)
                .ok_or_else(|| {
                    SkillCatalogError::InvalidInvocation(format!(
                        "Skill identity `{}` is not offered in this Provider and Workspace",
                        invocation.skill_id
                    ))
                })?;
            if descriptor.name != invocation.name || descriptor.scope != invocation.scope {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill identity `{}` does not match its safe metadata",
                    invocation.skill_id
                )));
            }
            let start = invocation.marker.start as usize;
            let end = invocation.marker.end as usize;
            let Some(marker) = prompt.text.get(start..end) else {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill `{}` has an invalid marker range",
                    invocation.name
                )));
            };
            let expected = format!("${}", descriptor.name);
            if !marker.eq_ignore_ascii_case(&expected) {
                return Err(SkillCatalogError::InvalidInvocation(format!(
                    "Skill `{}` is not bound to its visible marker",
                    invocation.name
                )));
            }
        }
        Ok(())
    }
}

fn canonical_workspace(workspace: &Path) -> Result<std::path::PathBuf, SkillCatalogError> {
    let workspace = fs::canonicalize(workspace).map_err(|_| SkillCatalogError::InvalidWorkspace)?;
    workspace
        .is_dir()
        .then_some(workspace)
        .ok_or(SkillCatalogError::InvalidWorkspace)
}

fn validate_catalog(
    catalog: &SkillCatalog,
    provider: &ProviderId,
    workspace: &Path,
) -> Result<(), SkillCatalogError> {
    if &catalog.provider != provider {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned a Skill Catalog owned by another Provider".to_owned(),
        ));
    }
    if catalog.workspace.path != workspace {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned a Skill Catalog for another Workspace".to_owned(),
        ));
    }
    let mut identities = HashSet::<&SkillId>::new();
    if catalog.skills.iter().any(|skill| {
        skill.name.is_empty()
            || skill.name.chars().any(char::is_whitespace)
            || !identities.insert(&skill.id)
    }) {
        return Err(SkillCatalogError::InvalidCatalog(
            "Provider returned invalid or duplicate Skill metadata".to_owned(),
        ));
    }
    Ok(())
}
