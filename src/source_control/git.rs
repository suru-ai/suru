use super::{SourceControl, repository_workspace};
use crate::protocol::*;
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
use tokio::process::Command;

/// Git command execution stays on its owning Server. Timeouts and the executable
/// are injectable so unavailable/hung installations need no global environment edits.
pub struct GitSourceControl {
    executable: PathBuf,
    timeout: Duration,
}
impl Default for GitSourceControl {
    fn default() -> Self {
        Self::new("git")
    }
}
impl GitSourceControl {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: Duration::from_secs(5),
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    async fn command(&self, directory: &Path, args: &[&str]) -> Result<Output, String> {
        let mut command = Command::new(&self.executable);
        command
            .arg("-C")
            .arg(directory)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        // Ambient Git overrides must not redirect discovery into another checkout.
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(name);
        }
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0");
        tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| "Git discovery timed out".to_owned())?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Git is not installed or cannot be found".to_owned()
                } else {
                    format!("Git could not run: {error}")
                }
            })
    }
    async fn text(&self, directory: &Path, args: &[&str]) -> Option<String> {
        self.command(directory, args)
            .await
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
    }
    async fn common(&self, directory: &Path) -> Option<PathBuf> {
        let path = self
            .text(
                directory,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?;
        std::fs::canonicalize(path).ok()
    }
    async fn valid_root(&self, path: &Path, common: &Path) -> Option<PathBuf> {
        let root = self.text(path, &["rev-parse", "--show-toplevel"]).await?;
        let root = std::fs::canonicalize(root).ok()?;
        let path = std::fs::canonicalize(path).ok()?;
        if root != path || self.common(&root).await.as_deref() != Some(common) {
            return None;
        }
        Some(root)
    }
}

#[async_trait]
impl SourceControl for GitSourceControl {
    fn reuse_discovery(
        &self,
        directory: &Path,
        previous: &ResolvedWorkspace,
    ) -> Option<ResolvedWorkspace> {
        let checkout = previous.checkout.as_ref()?;
        if !directory.is_dir() || !directory.starts_with(&checkout.root) {
            return None;
        }
        // Stop at nested Repository markers: the nearest Git Repository owns
        // the directory even when a parent checkout was already discovered.
        if directory
            .ancestors()
            .take_while(|parent| *parent != checkout.root)
            .any(|parent| {
                parent.join(".git").exists()
                    || (parent.join("HEAD").exists() && parent.join("objects").is_dir())
            })
        {
            return None;
        }
        let mut reading = previous.clone();
        reading.execution_directory = Some(ExecutionDirectory {
            path: directory.to_owned(),
        });
        Some(reading)
    }
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        let path = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_owned());
        let mut resolved = ResolvedWorkspace::directory(path.clone());
        if !path.is_dir() {
            resolved.workspace.source_control = SourceControlAvailability::Unavailable {
                reason: "Execution Directory is missing or unreadable".to_owned(),
            };
            return resolved;
        }
        let probe = match self
            .command(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
        {
            Ok(output) => output,
            Err(reason) => {
                resolved.workspace.source_control =
                    SourceControlAvailability::Unavailable { reason };
                return resolved;
            }
        };
        if !probe.status.success() {
            let markers = path.ancestors().any(|parent| {
                parent.join(".git").exists()
                    || (parent.join("HEAD").exists() && parent.join("objects").is_dir())
            });
            if markers {
                resolved.workspace.source_control = SourceControlAvailability::Unavailable {
                    reason: format!(
                        "Git Repository discovery failed: {}",
                        String::from_utf8_lossy(&probe.stderr).trim()
                    ),
                };
            }
            return resolved;
        }
        let common = match String::from_utf8(probe.stdout)
            .ok()
            .and_then(|text| std::fs::canonicalize(text.trim_end_matches(['\r', '\n'])).ok())
        {
            Some(path) => path,
            None => {
                resolved.workspace.source_control = SourceControlAvailability::Unavailable {
                    reason: "Git shared metadata is unreadable".to_owned(),
                };
                return resolved;
            }
        };
        let id = RepositoryId::from_metadata("git", &common);
        // Bare is a Repository property, not the linked checkout's rev-parse reading.
        let bare = self
            .text(&common, &["rev-parse", "--is-bare-repository"])
            .await
            .as_deref()
            == Some("true");
        let git_dir = self
            .text(&path, &["rev-parse", "--absolute-git-dir"])
            .await
            .and_then(|path| std::fs::canonicalize(path).ok());
        let top = self
            .text(&path, &["rev-parse", "--show-toplevel"])
            .await
            .and_then(|path| std::fs::canonicalize(path).ok());
        let mut location = if bare {
            RepositoryLocation::Bare {
                root: common.clone(),
            }
        } else {
            RepositoryLocation::UnknownMain
        };
        if !bare
            && git_dir.as_deref() == Some(&common)
            && let Some(root) = &top
        {
            location = RepositoryLocation::Main { root: root.clone() };
        }
        let mut checkouts = Vec::new();
        let listing = self
            .command(&path, &["worktree", "list", "--porcelain", "-z"])
            .await;
        let mut availability = SourceControlAvailability::Available;
        match listing {
            Ok(output) if output.status.success() => {
                for (index, entry) in parse_worktrees(&output.stdout).into_iter().enumerate() {
                    if entry.bare {
                        continue;
                    }
                    // Separate metadata layouts can report the metadata directory
                    // as the main worktree. It is a label hint, never executable proof.
                    let is_main = index == 0;
                    let root = if is_main {
                        if bare {
                            continue;
                        }
                        match &location {
                            RepositoryLocation::Main { root } => root.clone(),
                            _ => match self.valid_root(&entry.root, &common).await {
                                Some(root) => {
                                    location = RepositoryLocation::Main { root: root.clone() };
                                    root
                                }
                                None => continue,
                            },
                        }
                    } else {
                        canonical_checkout_path(&entry.root)
                    };
                    let valid = self.valid_root(&root, &common).await.is_some();
                    let association = CheckoutAssociation {
                        id: CheckoutId::from_root(&id, &root),
                        repository: id.clone(),
                        root,
                        kind: if is_main {
                            CheckoutKind::Main
                        } else {
                            CheckoutKind::Linked
                        },
                    };
                    checkouts.push(CheckoutSummary {
                        association,
                        revision: entry.revision,
                        availability: if valid {
                            SourceControlAvailability::Available
                        } else {
                            SourceControlAvailability::Unavailable {
                                reason: "Worktree is missing or unreadable".to_owned(),
                            }
                        },
                    });
                }
            }
            Ok(output) => {
                availability = SourceControlAvailability::Unavailable {
                    reason: format!(
                        "Git Worktree discovery failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                }
            }
            Err(reason) => availability = SourceControlAvailability::Unavailable { reason },
        }
        let checkout = top.map(|top| {
            checkouts
                .iter()
                .find(|checkout| checkout.association.root == top)
                .map(|checkout| checkout.association.clone())
                .unwrap_or_else(|| CheckoutAssociation {
                    id: CheckoutId::from_root(&id, &top),
                    repository: id.clone(),
                    root: top,
                    kind: if git_dir.as_deref() == Some(&common) {
                        CheckoutKind::Main
                    } else {
                        CheckoutKind::Linked
                    },
                })
        });
        let mut capabilities = SourceControlCapabilities::discovery_only();
        if let SourceControlAvailability::Unavailable { reason } = &availability {
            capabilities.list_checkouts = SourceControlCapability::Unsupported {
                reason: reason.clone(),
            };
        }
        let repository = Repository {
            id,
            system: "git".to_owned(),
            metadata_directory: common,
            location,
            availability,
            capabilities,
        };
        resolved.workspace = repository_workspace(&repository);
        resolved.checkout = checkout;
        // Repository metadata is a grouping context, never an execution fallback.
        if resolved.checkout.is_none() {
            resolved.execution_directory = None;
        }
        resolved.checkouts = checkouts;
        resolved
    }
}

// Canonicalize the surviving ancestor of a missing checkout too. In
// particular, Windows Git's C:/ spelling must keep the same canonical prefix
// as an association persisted while that checkout still existed.
fn canonical_checkout_path(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|ancestor| {
            let root = std::fs::canonicalize(ancestor).ok()?;
            Some(root.join(path.strip_prefix(ancestor).ok()?))
        })
        .unwrap_or_else(|| path.to_owned())
}

struct Entry {
    root: PathBuf,
    bare: bool,
    revision: Option<CheckoutRevision>,
}
fn parse_worktrees(bytes: &[u8]) -> Vec<Entry> {
    let mut result = Vec::new();
    let mut root = None;
    let mut bare = false;
    let mut branch = None;
    let mut commit = None;
    for field in bytes
        .split(|byte| *byte == 0)
        .chain(std::iter::once(&b""[..]))
    {
        if field.is_empty() {
            if let Some(root) = root.take() {
                let revision = if let Some(name) = branch.take() {
                    Some(CheckoutRevision::Branch {
                        name,
                        commit: commit.take(),
                    })
                } else {
                    commit
                        .take()
                        .map(|commit| CheckoutRevision::Detached { commit })
                };
                result.push(Entry {
                    root,
                    bare,
                    revision,
                });
                bare = false;
            }
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                root = Some(PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())));
            }
            #[cfg(not(unix))]
            {
                root = Some(PathBuf::from(String::from_utf8_lossy(path).into_owned()));
            }
        } else if field == b"bare" {
            bare = true;
        } else if let Some(name) = field.strip_prefix(b"branch refs/heads/") {
            branch = Some(String::from_utf8_lossy(name).into_owned());
        } else if let Some(hash) = field.strip_prefix(b"HEAD ")
            && hash.iter().any(|byte| *byte != b'0')
        {
            commit = Some(String::from_utf8_lossy(hash).into_owned());
        }
    }
    result
}
