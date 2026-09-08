//! Owning-Server source control discovery, with no Agent Provider dependencies.
use crate::protocol::{
    Repository, RepositoryId, RepositoryLocation, ResolvedWorkspace, SourceControlAvailability,
    Workspace,
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
mod git;
pub use git::GitSourceControl;

#[async_trait]
pub trait SourceControl: Send + Sync {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace;
    /// Reuse a reading within one discovery batch only when the adapter can
    /// establish that this directory has the same nearest checkout.
    fn reuse_discovery(
        &self,
        _directory: &Path,
        _previous: &ResolvedWorkspace,
    ) -> Option<ResolvedWorkspace> {
        None
    }
}

#[derive(Clone)]
pub(crate) struct SourceControlService {
    adapter: Arc<dyn SourceControl>,
    repositories: Arc<Mutex<HashMap<RepositoryId, Repository>>>,
}

impl SourceControlService {
    pub(crate) fn new(adapter: Arc<dyn SourceControl>) -> Self {
        Self {
            adapter,
            repositories: Default::default(),
        }
    }
    pub(crate) fn remember(&self, workspace: &Workspace) {
        if let Some(repository) = &workspace.repository {
            self.repositories
                .lock()
                .unwrap()
                .entry(repository.id.clone())
                .and_modify(|known| {
                    if matches!(known.location, RepositoryLocation::UnknownMain)
                        && !matches!(repository.location, RepositoryLocation::UnknownMain)
                    {
                        known.location = repository.location.clone();
                    }
                })
                .or_insert_with(|| repository.clone());
        }
    }
    pub(crate) fn workspaces(&self) -> Vec<Workspace> {
        self.repositories
            .lock()
            .unwrap()
            .values()
            .map(repository_workspace)
            .collect()
    }
    pub(crate) fn reuse_discovery(
        &self,
        directory: &Path,
        previous: &ResolvedWorkspace,
    ) -> Option<ResolvedWorkspace> {
        self.adapter.reuse_discovery(directory, previous)
    }
    pub(crate) async fn resolve(
        &self,
        directory: &Path,
        known: Option<&Workspace>,
    ) -> ResolvedWorkspace {
        let mut resolved = self.adapter.discover(directory).await;
        if resolved.workspace.repository.is_none()
            && let Some(repository) = known.and_then(|known| known.repository.as_ref())
        {
            let remembered = self.adapter.discover(&repository.metadata_directory).await;
            if remembered
                .workspace
                .repository
                .as_ref()
                .is_some_and(|discovered| discovered.id == repository.id)
            {
                resolved.workspace = remembered.workspace;
                resolved.checkouts = remembered.checkouts;
            }
        }
        if resolved.workspace.repository.is_none() {
            if let Some(known) = known.filter(|known| known.repository.is_some()) {
                // No membership is guessed from a missing path's spelling. Only a
                // previously persisted association keeps an unavailable Repository known.
                let reason = match &resolved.workspace.source_control {
                    SourceControlAvailability::Unavailable { reason } => reason.clone(),
                    _ => "The known Repository is no longer readable at this Execution Directory"
                        .to_owned(),
                };
                resolved.workspace = known.clone();
                resolved.workspace.source_control = SourceControlAvailability::Unavailable {
                    reason: reason.clone(),
                };
                let repository = resolved.workspace.repository.as_mut().unwrap();
                repository.capabilities.list_checkouts =
                    crate::protocol::SourceControlCapability::Unsupported {
                        reason: reason.clone(),
                    };
                repository.availability = SourceControlAvailability::Unavailable { reason };
            }
        }
        // Git cannot name a separately located main checkout from its metadata.
        // A previously observed root can be revalidated without treating the label
        // itself as proof that a working copy still exists.
        let remembered_main = resolved
            .workspace
            .repository
            .as_ref()
            .and_then(|repository| {
                if !matches!(repository.location, RepositoryLocation::UnknownMain) {
                    return None;
                }
                self.repositories
                    .lock()
                    .unwrap()
                    .get(&repository.id)
                    .and_then(|previous| match &previous.location {
                        RepositoryLocation::Main { root } => {
                            Some((repository.id.clone(), root.clone()))
                        }
                        _ => None,
                    })
            });
        if let Some((id, root)) = remembered_main {
            let main = self.adapter.discover(&root).await;
            if main
                .workspace
                .repository
                .as_ref()
                .is_some_and(|repository| repository.id == id)
            {
                resolved.workspace = main.workspace;
                resolved.checkouts = main.checkouts;
            } else if !resolved
                .checkouts
                .iter()
                .any(|checkout| checkout.association.root == root)
            {
                resolved.checkouts.push(crate::protocol::CheckoutSummary {
                    association: crate::protocol::CheckoutAssociation {
                        id: crate::protocol::CheckoutId::from_root(&id, &root),
                        repository: id,
                        root,
                        kind: crate::protocol::CheckoutKind::Main,
                    },
                    revision: None,
                    availability: SourceControlAvailability::Unavailable {
                        reason: "The known main checkout is missing or unreadable".to_owned(),
                    },
                });
            }
        }
        if let Some(repository) = &mut resolved.workspace.repository {
            let mut repositories = self.repositories.lock().unwrap();
            if matches!(repository.location, RepositoryLocation::UnknownMain)
                && let Some(previous) = repositories.get(&repository.id)
                && !matches!(previous.location, RepositoryLocation::UnknownMain)
            {
                repository.location = previous.location.clone();
            }
            repositories.insert(repository.id.clone(), repository.clone());
            resolved.workspace = repository_workspace(repository);
        }
        resolved
    }
}

pub(crate) fn repository_workspace(repository: &Repository) -> Workspace {
    Workspace {
        id: repository.id.workspace_id(),
        path: repository.presentation_path().to_owned(),
        repository: Some(repository.clone()),
        source_control: repository.availability.clone(),
    }
}
