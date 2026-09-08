//! Source control facts owned by a Server, independent of Agent Providers.
use super::{ExecutionDirectory, Workspace};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl WorkspaceId {
    pub fn directory(path: &Path) -> Self {
        Self(identity("directory", path))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RepositoryId(pub String);

impl RepositoryId {
    pub fn from_metadata(system: &str, path: &Path) -> Self {
        Self(identity(system, path))
    }
    pub fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId(self.0.clone())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CheckoutId(pub String);

impl CheckoutId {
    pub fn from_root(repository: &RepositoryId, root: &Path) -> Self {
        Self(identity(&repository.0, root))
    }
}

fn identity(namespace: &str, path: &Path) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(namespace.as_bytes());
    hash.update(b"\0");
    hash.update(path.as_os_str().as_encoded_bytes());
    format!("{namespace}:{}", hash.finalize().to_hex())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceControlAvailability {
    Available,
    NotDetected,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceControlCapability {
    Available,
    Unsupported { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceControlCapabilities {
    pub list_checkouts: SourceControlCapability,
    pub create_checkout: SourceControlCapability,
    pub recover_checkout: SourceControlCapability,
    pub remove_checkout: SourceControlCapability,
}

impl SourceControlCapabilities {
    pub fn discovery_only() -> Self {
        let unsupported = || SourceControlCapability::Unsupported {
            reason: "This source control operation is not implemented".to_owned(),
        };
        Self {
            list_checkouts: SourceControlCapability::Available,
            create_checkout: unsupported(),
            recover_checkout: unsupported(),
            remove_checkout: unsupported(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryLocation {
    Main { root: PathBuf },
    Bare { root: PathBuf },
    UnknownMain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repository {
    pub id: RepositoryId,
    pub system: String,
    pub metadata_directory: PathBuf,
    pub location: RepositoryLocation,
    pub availability: SourceControlAvailability,
    pub capabilities: SourceControlCapabilities,
}

impl Repository {
    pub fn presentation_path(&self) -> &Path {
        match &self.location {
            RepositoryLocation::Main { root } | RepositoryLocation::Bare { root } => root,
            RepositoryLocation::UnknownMain => &self.metadata_directory,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutKind {
    Main,
    Linked,
}

/// Durable association, not a Session-owned working copy or historical branch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckoutAssociation {
    /// Latest successful reading for recovery; never a live display value.
    #[serde(default)]
    pub recovery_revision: Option<CheckoutRevision>,
    pub id: CheckoutId,
    pub repository: RepositoryId,
    pub root: PathBuf,
    pub kind: CheckoutKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckoutRevision {
    Branch {
        name: String,
        commit: Option<String>,
    },
    Detached {
        commit: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckoutSummary {
    pub association: CheckoutAssociation,
    pub revision: Option<CheckoutRevision>,
    pub availability: SourceControlAvailability,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecutionDirectoryStatus {
    Available,
    Unavailable { reason: String },
    RequiresWorkingCopy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedWorkspace {
    pub execution_status: ExecutionDirectoryStatus,
    pub workspace: Workspace,
    pub execution_directory: Option<ExecutionDirectory>,
    pub checkout: Option<CheckoutAssociation>,
    pub checkouts: Vec<CheckoutSummary>,
}

impl ResolvedWorkspace {
    pub fn directory(path: PathBuf) -> Self {
        Self {
            execution_status: ExecutionDirectoryStatus::Available,
            workspace: Workspace::directory(path.clone()),
            execution_directory: Some(ExecutionDirectory { path }),
            checkout: None,
            checkouts: Vec::new(),
        }
    }
}

/// Stable working-copy preparation identity; independent of edited Prompt IDs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PreparationId(pub uuid::Uuid);
impl Default for PreparationId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareCheckoutRequest {
    pub id: PreparationId,
    pub source: ExecutionDirectory,
    pub description: String,
    pub provider: super::ProviderId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedCheckout {
    pub id: PreparationId,
    pub source: ExecutionDirectory,
    pub repository: Repository,
    pub destination: ExecutionDirectory,
    /// Adapter-owned creation facts (Git uses a branch and immutable commit).
    pub plan: CheckoutPreparationPlan,
    pub checkout_created: bool,
    pub ready: bool,
    pub intended_session: super::SessionId,
    pub admitted_session: Option<super::SessionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrepareCheckoutResult {
    pub preparation: PreparedCheckout,
    pub location: Option<ResolvedWorkspace>,
    pub error: Option<String>,
}

/// Concrete adapter plans can evolve independently of shared preparation state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "system", rename_all = "snake_case")]
pub enum CheckoutPreparationPlan {
    Git {
        branch: String,
        source_commit: String,
    },
}

/// Owning-Server validation/recreation result for a retained working copy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckoutRecovery {
    pub checkout: CheckoutAssociation,
    pub recreated: bool,
}
