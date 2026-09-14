//! Owning-Server source control discovery, with no Agent Provider dependencies.
use crate::protocol::{
    Repository, RepositoryId, RepositoryLocation, ResolvedWorkspace, SourceControlAvailability,
    Workspace,
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
mod git;
mod preparation;
pub use git::GitSourceControl;
pub(crate) use preparation::PreparationStore;

/// Observable preparation boundaries, injectable for interruption testing and hosts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationCheckpoint {
    IntentPersisted,
    BranchCreated,
    RegistrationCreated,
    CheckoutCreated,
    SessionPersisted,
    Admitted,
    IntentRetired,
}

#[async_trait]
pub trait PreparationObserver: Send + Sync {
    async fn checkpoint(
        &self,
        at: PreparationCheckpoint,
        preparation: &crate::protocol::PreparedCheckout,
    ) -> Result<(), String>;
}

#[async_trait]
pub trait SourceControl: Send + Sync {
    async fn checkpoint(
        &self,
        _at: PreparationCheckpoint,
        _preparation: &crate::protocol::PreparedCheckout,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn inspect_removal(
        &self,
        _target: &crate::protocol::CheckoutRemovalTarget,
    ) -> Result<crate::protocol::CheckoutRemovalInspection, String> {
        Err("Working-copy removal is unsupported".into())
    }
    async fn removal_branch_outcome(
        &self,
        _target: &crate::protocol::CheckoutRemovalTarget,
        _inspection: &crate::protocol::CheckoutRemovalInspection,
    ) -> Result<crate::protocol::CheckoutBranchOutcome, String> {
        Ok(crate::protocol::CheckoutBranchOutcome::Retained)
    }
    async fn remove_checkout(
        &self,
        _target: &crate::protocol::CheckoutRemovalTarget,
        _inspection: &crate::protocol::CheckoutRemovalInspection,
        _force: bool,
        _branch_outcome: crate::protocol::CheckoutBranchOutcome,
    ) -> Result<crate::protocol::CheckoutBranchOutcome, String> {
        Err("Working-copy removal is unsupported".into())
    }
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace;
    async fn plan_checkout(
        &self,
        _id: crate::protocol::PreparationId,
        _source: &ResolvedWorkspace,
        _description: &str,
    ) -> Result<crate::protocol::PreparedCheckout, String> {
        Err("Working-copy creation is unsupported".to_owned())
    }
    async fn prepare_checkout(
        &self,
        _plan: &crate::protocol::PreparedCheckout,
    ) -> Result<ResolvedWorkspace, String> {
        Err("Working-copy creation is unsupported".to_owned())
    }

    /// Validate a retained working copy, restoring an absent one when supported.
    /// Report recreation after all adapter-owned initialization succeeds, including
    /// when resuming a partial attempt, so every native connection can reconnect.
    async fn recover_checkout(
        &self,
        _repository: &Repository,
        checkout: &crate::protocol::CheckoutAssociation,
    ) -> Result<crate::protocol::CheckoutRecovery, String> {
        let reading = self.observe(checkout).await;
        if matches!(reading.availability, SourceControlAvailability::Available) {
            Ok(crate::protocol::CheckoutRecovery {
                checkout: checkout.clone(),
                recreated: false,
            })
        } else {
            Err(
                "Working-copy recovery is unsupported; restore the known checkout before retrying"
                    .to_owned(),
            )
        }
    }
    /// Read the known working copy, never replacing its identity with a new
    /// repository that happens to occupy the same path.
    async fn observe(
        &self,
        checkout: &crate::protocol::CheckoutAssociation,
    ) -> crate::protocol::CheckoutSummary {
        let resolved = self.discover(&checkout.root).await;
        resolved
            .checkouts
            .into_iter()
            .find(|reading| reading.association.id == checkout.id)
            .unwrap_or_else(|| crate::protocol::CheckoutSummary {
                association: checkout.clone(),
                revision: None,
                availability: SourceControlAvailability::Unavailable {
                    reason: "The known checkout is missing or unreadable".to_owned(),
                },
            })
    }
    /// Every Worktree this Repository presently has, named rather than read.
    /// Checkout observation enumerates a known Repository through this on its
    /// own cadence, so a Worktree added or removed outside Suru is picked up;
    /// it is one call per Repository, and each named Worktree is then read by
    /// [`SourceControl::observe`].
    ///
    /// A listing that could not be taken is an error rather than an empty
    /// Repository, so that a source control system briefly unable to answer is
    /// never mistaken for every Worktree having gone.
    async fn list_checkouts(
        &self,
        repository: &Repository,
    ) -> Result<Vec<crate::protocol::CheckoutAssociation>, String> {
        Ok(self
            .discover(repository.presentation_path())
            .await
            .checkouts
            .into_iter()
            .filter(|reading| reading.association.repository == repository.id)
            .map(|reading| reading.association)
            .collect())
    }
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
    mutations: Arc<Mutex<HashMap<RepositoryId, Arc<tokio::sync::Mutex<()>>>>>,
    incarnations: Arc<Mutex<HashMap<crate::protocol::CheckoutId, u64>>>,
}

impl SourceControlService {
    pub(crate) fn new(adapter: Arc<dyn SourceControl>) -> Self {
        Self {
            adapter,
            repositories: Default::default(),
            mutations: Default::default(),
            incarnations: Default::default(),
        }
    }
    pub(crate) async fn inspect_removal(
        &self,
        target: &crate::protocol::CheckoutRemovalTarget,
    ) -> Result<crate::protocol::CheckoutRemovalInspection, String> {
        self.adapter.inspect_removal(target).await
    }
    pub(crate) async fn removal_branch_outcome(
        &self,
        target: &crate::protocol::CheckoutRemovalTarget,
        inspection: &crate::protocol::CheckoutRemovalInspection,
    ) -> Result<crate::protocol::CheckoutBranchOutcome, String> {
        self.adapter
            .removal_branch_outcome(target, inspection)
            .await
    }
    pub(crate) async fn remove_checkout(
        &self,
        target: &crate::protocol::CheckoutRemovalTarget,
        inspection: &crate::protocol::CheckoutRemovalInspection,
        force: bool,
        branch_outcome: crate::protocol::CheckoutBranchOutcome,
    ) -> Result<crate::protocol::CheckoutBranchOutcome, String> {
        self.adapter
            .remove_checkout(target, inspection, force, branch_outcome)
            .await
    }
    pub(crate) async fn mutation_guard(
        &self,
        id: &RepositoryId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .mutations
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        lock.lock_owned().await
    }
    pub(crate) async fn prepare_execution(
        &self,
        session: &crate::protocol::Session,
        guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<ExecutionLease, String> {
        use crate::protocol::*;
        let Some(checkout) = &session.checkout else {
            return Ok(ExecutionLease {
                reading: None,
                guard,
                incarnation: 0,
                recreated: false,
            });
        };
        let repository = session
            .workspace
            .repository
            .as_ref()
            .ok_or("Known checkout has no Repository association")?;
        let guard = match guard {
            Some(guard) => guard,
            None => self.mutation_guard(&repository.id).await,
        };
        let recovered = if checkout.kind == CheckoutKind::Linked {
            self.adapter.recover_checkout(repository, checkout).await?
        } else {
            // Main working copies are validated by exact discovery below;
            // background observation is reserved for catalog interest.
            CheckoutRecovery {
                checkout: checkout.clone(),
                recreated: false,
            }
        };
        // Every Session sharing this root must see a recreation even if the
        // triggering Session subsequently fails its own subdirectory check.
        let incarnation = {
            let mut incarnations = self.incarnations.lock().unwrap();
            let incarnation = incarnations.entry(checkout.id.clone()).or_default();
            if recovered.recreated {
                *incarnation = incarnation.saturating_add(1);
            }
            *incarnation
        };
        let path = crate::paths::canonical(&session.execution_directory.path).map_err(|_| format!("The exact Execution Directory {} is unavailable after Worktree recovery; choose another Session or restore this subdirectory", session.execution_directory.path.display()))?;
        if !path.is_dir()
            || !path.starts_with(&checkout.root)
            || path != session.execution_directory.path
        {
            return Err(
                "The Session's exact Execution Directory changed; execution was not redirected"
                    .to_owned(),
            );
        }
        let exact = self.adapter.discover(&path).await;
        if exact.workspace.id != session.workspace.id
            || exact
                .checkout
                .as_ref()
                .is_none_or(|actual| actual.id != checkout.id)
        {
            return Err("The exact Execution Directory now belongs to a different Repository or checkout; execution was not redirected".to_owned());
        }
        let association = exact.checkout.expect("validated checkout identity");
        let reading = CheckoutSummary {
            revision: association.recovery_revision.clone(),
            association,
            availability: SourceControlAvailability::Available,
        };
        Ok(ExecutionLease {
            reading: Some(reading),
            guard: Some(guard),
            incarnation,
            recreated: recovered.recreated,
        })
    }
    pub(crate) async fn checkpoint(
        &self,
        at: PreparationCheckpoint,
        plan: &crate::protocol::PreparedCheckout,
    ) -> Result<(), String> {
        self.adapter.checkpoint(at, plan).await
    }
    pub(crate) async fn plan_checkout(
        &self,
        request: &crate::protocol::PrepareCheckoutRequest,
    ) -> Result<
        (
            crate::protocol::PreparedCheckout,
            tokio::sync::OwnedMutexGuard<()>,
        ),
        String,
    > {
        let source = self.resolve(&request.source.path, None).await;
        let repository = source
            .workspace
            .repository
            .as_ref()
            .ok_or("A Repository is required")?;
        let guard = self.mutation_guard(&repository.id).await;
        let current = self
            .resolve(&request.source.path, Some(&source.workspace))
            .await;
        if current.workspace.repository.as_ref().map(|repo| &repo.id) != Some(&repository.id) {
            return Err("Source Repository changed during preparation".into());
        }
        let plan = self
            .adapter
            .plan_checkout(request.id, &current, &request.description)
            .await?;
        Ok((plan, guard))
    }
    pub(crate) async fn prepare_checkout(
        &self,
        plan: &crate::protocol::PreparedCheckout,
    ) -> Result<ResolvedWorkspace, String> {
        self.adapter.prepare_checkout(plan).await
    }
    pub(crate) async fn observe(
        &self,
        checkout: &crate::protocol::CheckoutAssociation,
    ) -> crate::protocol::CheckoutSummary {
        self.adapter.observe(checkout).await
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
    /// Every Repository this Server has grouped a Workspace for, however it came
    /// to know it — through a Session at startup, or through a Workspace
    /// resolution since. Checkout observation watches all of their Worktrees,
    /// including the ones no Session works in.
    pub(crate) fn repositories(&self) -> Vec<Repository> {
        self.repositories
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }
    pub(crate) async fn list_checkouts(
        &self,
        repository: &Repository,
    ) -> Result<Vec<crate::protocol::CheckoutAssociation>, String> {
        self.adapter.list_checkouts(repository).await
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
    /// Selection has a different contract from explicit path entry: a Client's
    /// remembered directory remains selected when unavailable, never substituted.
    pub(crate) async fn resolve_selection(
        &self,
        named: &Path,
        known: Option<&Workspace>,
        request: &crate::protocol::ResolveWorkspaceRequest,
    ) -> Result<ResolvedWorkspace, String> {
        use crate::protocol::{
            ExecutionDirectory, ExecutionDirectoryStatus, SourceControlAvailability,
        };
        if (request.checkout_id.is_some() || request.remembered_execution_directory.is_some())
            && request.workspace_id.is_none()
        {
            return Err("Choose a Workspace before selecting an execution location".to_owned());
        }
        if request.checkout_id.is_some() && request.remembered_execution_directory.is_some() {
            return Err("Choose one execution location".to_owned());
        }
        let path = crate::paths::canonical(named).unwrap_or_else(|_| named.to_owned());
        if request.workspace_id.is_none() && !path.is_dir() {
            return Err(if path.exists() {
                "Not a directory"
            } else {
                "No directory there"
            }
            .to_owned());
        }
        let mut resolved = self.resolve(&path, known).await;
        if request
            .workspace_id
            .as_ref()
            .is_some_and(|id| id != &resolved.workspace.id)
        {
            return Err("The selected directory no longer belongs to this Workspace".to_owned());
        }
        if let Some(id) = &request.checkout_id {
            let checkout = resolved
                .checkouts
                .iter()
                .find(|checkout| &checkout.association.id == id)
                .ok_or_else(|| "This Worktree is no longer known to its Repository".to_owned())?;
            if let SourceControlAvailability::Unavailable { reason } = &checkout.availability {
                return Err(reason.clone());
            }
            let selected = self.resolve(&checkout.association.root, None).await;
            if selected.workspace.id != resolved.workspace.id
                || selected
                    .checkout
                    .as_ref()
                    .is_none_or(|checkout| &checkout.id != id)
                || selected
                    .execution_directory
                    .as_ref()
                    .is_none_or(|directory| directory.path != checkout.association.root)
            {
                return Err(
                    "The selected Worktree is missing or no longer belongs to this Repository"
                        .to_owned(),
                );
            }
            return Ok(selected);
        }
        if let Some(directory) = &request.remembered_execution_directory {
            // Revalidate actual membership without restoring a durable association:
            // an unrelated replacement directory must not inherit its old Repository.
            let remembered = self.adapter.discover(&directory.path).await;
            resolved.execution_directory = Some(ExecutionDirectory {
                path: directory.path.clone(),
            });
            resolved.execution_status = if let ExecutionDirectoryStatus::Unavailable { reason } =
                &remembered.execution_status
            {
                ExecutionDirectoryStatus::Unavailable {
                    reason: reason.clone(),
                }
            } else if remembered.workspace.id != resolved.workspace.id {
                ExecutionDirectoryStatus::Unavailable {
                    reason: "The remembered directory no longer belongs to this Workspace"
                        .to_owned(),
                }
            } else {
                remembered.execution_status
            };
            resolved.checkout = if remembered.workspace.id == resolved.workspace.id {
                remembered.checkout
            } else {
                None
            };
            // Keep the original spelling when missing, canonicalize only when
            // the addressed Server could still read this exact directory.
            if !matches!(
                resolved.execution_status,
                ExecutionDirectoryStatus::Unavailable { .. }
            ) {
                resolved.execution_directory = remembered.execution_directory;
            }
        }
        if request.remembered_execution_directory.is_none()
            && resolved.execution_directory.is_some()
            && resolved.workspace.repository.is_some()
        {
            let actual = self.adapter.discover(&path).await;
            if actual.workspace.id != resolved.workspace.id {
                resolved.execution_status = ExecutionDirectoryStatus::Unavailable {
                    reason:
                        "The known main checkout is missing or no longer belongs to this Repository"
                            .to_owned(),
                };
                resolved.checkout = None;
            }
        }
        Ok(resolved)
    }

    pub(crate) async fn resolve(
        &self,
        directory: &Path,
        known: Option<&Workspace>,
    ) -> ResolvedWorkspace {
        self.resolve_in_batch(&mut DiscoveryBatch::default(), directory, known)
            .await
    }
    /// Resolve within one discovery batch, so that several directories which
    /// fall back to the same Repository metadata or main checkout are read from
    /// source control once rather than once per directory.
    pub(crate) async fn resolve_in_batch(
        &self,
        batch: &mut DiscoveryBatch,
        directory: &Path,
        known: Option<&Workspace>,
    ) -> ResolvedWorkspace {
        let mut resolved = batch.discover(self.adapter.as_ref(), directory).await;
        if resolved.workspace.repository.is_none()
            && let Some(repository) = known.and_then(|known| known.repository.as_ref())
        {
            // A checkout of the known Repository already read in this batch
            // is a better reading of it than its bare metadata directory.
            let remembered = match batch.reading_of(&repository.id) {
                Some(reading) => reading,
                None => {
                    batch
                        .discover(self.adapter.as_ref(), &repository.metadata_directory)
                        .await
                }
            };
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
            let main = batch.discover(self.adapter.as_ref(), &root).await;
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
                        recovery_revision: None,
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

/// Adapter readings memoized for the span of one discovery batch. Readings are
/// keyed by the path handed to the adapter, so the batch never guesses that
/// two spellings of a directory are the same place.
#[derive(Default)]
pub(crate) struct DiscoveryBatch {
    readings: HashMap<PathBuf, ResolvedWorkspace>,
}

impl DiscoveryBatch {
    async fn discover(&mut self, adapter: &dyn SourceControl, path: &Path) -> ResolvedWorkspace {
        if let Some(reading) = self.readings.get(path) {
            return reading.clone();
        }
        let reading = adapter.discover(path).await;
        self.readings.insert(path.to_owned(), reading.clone());
        reading
    }
    fn reading_of(&self, repository: &RepositoryId) -> Option<ResolvedWorkspace> {
        self.readings
            .values()
            .find(|reading| {
                reading
                    .workspace
                    .repository
                    .as_ref()
                    .is_some_and(|known| &known.id == repository)
            })
            .cloned()
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

/// Holds the Repository mutation barrier through one native admission.
pub(crate) struct ExecutionLease {
    pub(crate) reading: Option<crate::protocol::CheckoutSummary>,
    pub(crate) guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    pub(crate) incarnation: u64,
    pub(crate) recreated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Knows one Repository by its metadata directory and nothing else.
    struct MetadataOnly {
        metadata: PathBuf,
        metadata_readings: AtomicUsize,
    }

    #[async_trait]
    impl SourceControl for MetadataOnly {
        async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
            let mut resolved = ResolvedWorkspace::directory(directory.to_owned());
            if directory == self.metadata {
                self.metadata_readings.fetch_add(1, Ordering::SeqCst);
                let repository = Repository {
                    id: RepositoryId::from_metadata("fake", &self.metadata),
                    system: "fake".to_owned(),
                    metadata_directory: self.metadata.clone(),
                    location: RepositoryLocation::Main {
                        root: self.metadata.clone(),
                    },
                    availability: SourceControlAvailability::Available,
                    capabilities: crate::protocol::SourceControlCapabilities::discovery_only(),
                };
                resolved.workspace = repository_workspace(&repository);
            } else {
                resolved.workspace.source_control = SourceControlAvailability::Unavailable {
                    reason: "missing".to_owned(),
                };
            }
            resolved
        }
    }

    #[tokio::test]
    async fn one_batch_reads_shared_repository_metadata_once() {
        let metadata = PathBuf::from("repository-metadata");
        let adapter = Arc::new(MetadataOnly {
            metadata: metadata.clone(),
            metadata_readings: AtomicUsize::new(0),
        });
        let service = SourceControlService::new(adapter.clone());
        let known = adapter.discover(&metadata).await.workspace;
        assert_eq!(adapter.metadata_readings.swap(0, Ordering::SeqCst), 1);

        let mut batch = DiscoveryBatch::default();
        service.resolve_in_batch(&mut batch, &metadata, None).await;
        assert_eq!(adapter.metadata_readings.load(Ordering::SeqCst), 1);
        for missing in ["missing-a", "missing-b", "missing-c"] {
            let resolved = service
                .resolve_in_batch(&mut batch, Path::new(missing), Some(&known))
                .await;
            let repository = resolved
                .workspace
                .repository
                .expect("the known Repository survives a missing directory");
            assert_eq!(repository.id, known.repository.as_ref().unwrap().id);
        }
        assert_eq!(
            adapter.metadata_readings.load(Ordering::SeqCst),
            1,
            "missing directories reuse the batch's reading of their Repository"
        );

        let mut batch = DiscoveryBatch::default();
        for missing in ["missing-a", "missing-b"] {
            service
                .resolve_in_batch(&mut batch, Path::new(missing), Some(&known))
                .await;
        }
        assert_eq!(
            adapter.metadata_readings.load(Ordering::SeqCst),
            2,
            "without a checkout reading, the metadata directory is read once per batch"
        );

        service.resolve(Path::new("missing-d"), Some(&known)).await;
        assert_eq!(
            adapter.metadata_readings.load(Ordering::SeqCst),
            3,
            "a fresh resolution outside the batch reads again"
        );
    }
}
