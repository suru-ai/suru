use super::{SourceControl, repository_workspace};
use crate::protocol::*;
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Mutex,
    time::Duration,
};
use tokio::process::Command;
mod preparation;
mod recovery;
mod removal;

/// Git command execution stays on its owning Server. Timeouts and the executable
/// are injectable so unavailable/hung installations need no global environment edits.
pub struct GitSourceControl {
    executable: PathBuf,
    timeout: Duration,
    mutation_timeout: Duration,
    configuration_file: Option<PathBuf>,
    observer: Option<std::sync::Arc<dyn super::PreparationObserver>>,
    /// Worktrees whose identity a prior observation validated with Git. Each
    /// observation poll then repeats the three identity reads only once the
    /// Worktree root no longer looks like the one Git confirmed.
    validated_roots: Mutex<HashMap<CheckoutId, ValidatedRoot>>,
}
struct ValidatedRoot {
    common: PathBuf,
    marker: GitMarker,
}
/// A spawn-free fingerprint of a Worktree root's `.git` entry. A linked
/// Worktree's file names its metadata directory, so an unrelated Worktree at
/// the same path changes the marker even when the old Repository's metadata
/// survives. A main Worktree's identity is its metadata path, so a Repository
/// re-initialized there is the same Repository, exactly as Git reports it.
#[derive(PartialEq)]
enum GitMarker {
    Directory,
    File(Vec<u8>),
}
fn git_marker(root: &Path) -> Option<GitMarker> {
    let entry = root.join(".git");
    let metadata = std::fs::symlink_metadata(&entry).ok()?;
    if metadata.is_dir() {
        Some(GitMarker::Directory)
    } else if metadata.is_file() {
        std::fs::read(&entry).ok().map(GitMarker::File)
    } else {
        None
    }
}
impl Default for GitSourceControl {
    fn default() -> Self {
        Self::new("git")
    }
}
impl GitSourceControl {
    async fn observe_checkout(
        &self,
        checkout: &CheckoutAssociation,
        include_recovering: bool,
    ) -> CheckoutSummary {
        let mut reading = CheckoutSummary {
            association: checkout.clone(),
            revision: None,
            availability: SourceControlAvailability::Unavailable {
                reason: "The known checkout is missing or unreadable".to_owned(),
            },
        };
        reading.association.recovery_revision = None;
        if !include_recovering && recovery_in_progress(&checkout.root) {
            reading.availability = SourceControlAvailability::Unavailable {
                reason: "Worktree recovery is incomplete; retry its retained Session".to_owned(),
            };
            return reading;
        }
        let revision = if self.still_validated(checkout) {
            self.read_revision(&checkout.root).await
        } else {
            None
        };
        let revision = match revision {
            Some(revision) => revision,
            // A read that fails after validation is re-validated in full so a
            // replaced or unreadable root is reported for what it now is.
            None => match self.validate_root(checkout).await {
                Some(revision) => revision,
                None => return reading,
            },
        };
        reading.revision = Some(revision);
        reading.availability = SourceControlAvailability::Available;
        reading
    }
    /// Confirm with Git that the root is this checkout's Worktree, remembering
    /// it for later polls, and read its revision.
    async fn validate_root(&self, checkout: &CheckoutAssociation) -> Option<CheckoutRevision> {
        self.validated_roots.lock().unwrap().remove(&checkout.id);
        // Fingerprint before Git confirms the identity: a root replaced in
        // between then mismatches its own marker and is validated again.
        let marker = git_marker(&checkout.root);
        let common = self.common(&checkout.root).await?;
        if RepositoryId::from_metadata("git", &common) != checkout.repository
            || self.valid_root(&checkout.root, &common).await.as_ref() != Some(&checkout.root)
        {
            return None;
        }
        let revision = self.read_revision(&checkout.root).await?;
        if let Some(marker) = marker {
            self.validated_roots
                .lock()
                .unwrap()
                .insert(checkout.id.clone(), ValidatedRoot { common, marker });
        }
        Some(revision)
    }
    /// Whether a Worktree still looks like the one Git validated for this
    /// checkout, judged without spawning Git: its metadata directory survives,
    /// its root is not a symlink onto somewhere else, and its `.git` entry is
    /// the one that was fingerprinted.
    fn still_validated(&self, checkout: &CheckoutAssociation) -> bool {
        let mut validated = self.validated_roots.lock().unwrap();
        let Some(known) = validated.get(&checkout.id) else {
            return false;
        };
        if known.common.is_dir()
            && RepositoryId::from_metadata("git", &known.common) == checkout.repository
            && crate::paths::canonical(&checkout.root).ok().as_ref() == Some(&checkout.root)
            && git_marker(&checkout.root).as_ref() == Some(&known.marker)
        {
            return true;
        }
        validated.remove(&checkout.id);
        false
    }
    /// The branch and commit of a validated root. `None` means Git could not
    /// read them; an unborn branch or a detached HEAD is still a revision.
    async fn read_revision(&self, root: &Path) -> Option<CheckoutRevision> {
        let Ok(branch_output) = self
            .command(root, &["symbolic-ref", "--quiet", "HEAD"])
            .await
        else {
            return None;
        };
        let branch = if branch_output.status.success() {
            let Ok(name) = String::from_utf8(branch_output.stdout) else {
                return None;
            };
            Some(name.trim_end_matches(['\r', '\n']).to_owned())
        } else if branch_output.status.code() == Some(1) {
            None // Detached HEAD is the documented quiet symbolic-ref miss.
        } else {
            return None;
        };
        let Ok(commit_output) = self
            .command(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await
        else {
            return None;
        };
        let commit = if commit_output.status.success() {
            let Ok(commit) = String::from_utf8(commit_output.stdout) else {
                return None;
            };
            Some(commit.trim_end_matches(['\r', '\n']).to_owned())
        } else if let Some(branch) = &branch {
            // Only a genuinely absent branch ref is unborn. A failed commit
            // read from an existing ref must not erase retained recovery facts.
            let Ok(reference) = self
                .command(root, &["show-ref", "--verify", "--quiet", branch])
                .await
            else {
                return None;
            };
            if reference.status.code() != Some(1) {
                return None;
            }
            None
        } else {
            return None;
        };
        match (branch, commit) {
            (Some(name), commit) => Some(CheckoutRevision::Branch {
                name: name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned(),
                commit,
            }),
            (None, Some(commit)) => Some(CheckoutRevision::Detached { commit }),
            (None, None) => None,
        }
    }

    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: Duration::from_secs(5),
            mutation_timeout: Duration::from_secs(120),
            configuration_file: None,
            observer: None,
            validated_roots: Mutex::new(HashMap::new()),
        }
    }
    pub fn with_preparation_observer(
        mut self,
        observer: std::sync::Arc<dyn super::PreparationObserver>,
    ) -> Self {
        self.observer = Some(observer);
        self
    }
    /// An isolated Git user configuration, useful for embedded hosts and fixtures.
    pub fn with_configuration_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.configuration_file = Some(path.into());
        self
    }
    pub fn with_mutation_timeout(mut self, timeout: Duration) -> Self {
        self.mutation_timeout = timeout;
        self
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    async fn command(&self, directory: &Path, args: &[&str]) -> Result<Output, String> {
        self.command_with_timeout(directory, args, self.timeout)
            .await
    }
    async fn command_with_timeout(
        &self,
        directory: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, String> {
        self.command_with_input(directory, args, timeout, None)
            .await
    }
    async fn command_with_input(
        &self,
        directory: &Path,
        args: &[&str],
        timeout: Duration,
        input: Option<&str>,
    ) -> Result<Output, String> {
        use std::io::Write;
        let stdin = if let Some(input) = input {
            use std::io::{Seek, SeekFrom};
            let mut file = tempfile::tempfile().map_err(|e| e.to_string())?;
            file.write_all(input.as_bytes())
                .map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            Stdio::from(file)
        } else {
            Stdio::null()
        };
        let mut command = Command::new(&self.executable);
        command
            .arg("-C")
            .arg(directory)
            .args(args)
            .stdin(stdin)
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            // The Server runs detached from any console, so a console-subsystem
            // Git would otherwise open a window for every observation poll.
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            command.creation_flags(CREATE_NO_WINDOW);
        }
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
        if let Some(path) = &self.configuration_file {
            command.env("GIT_CONFIG_GLOBAL", path);
        }
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0");
        tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| "Git operation timed out".to_owned())?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Git is not installed or cannot be found".to_owned()
                } else {
                    format!("Git could not run: {error}")
                }
            })
    }
    async fn mutate(&self, directory: &Path, args: &[&str]) -> Result<(), String> {
        let output = self
            .command_with_timeout(directory, args, self.mutation_timeout)
            .await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
    async fn validate_prepared(&self, plan: &PreparedCheckout) -> Result<(), String> {
        if std::fs::symlink_metadata(&plan.destination.path)
            .is_ok_and(|m| m.file_type().is_symlink())
            || self
                .valid_root(&plan.destination.path, &plan.repository.metadata_directory)
                .await
                .as_ref()
                != Some(&plan.destination.path)
        {
            return Err("The prepared destination is occupied by unrelated contents; it was not overwritten".to_owned());
        }
        let CheckoutPreparationPlan::Git { branch: name, .. } = &plan.plan;
        if self
            .text(&plan.destination.path, &["symbolic-ref", "--short", "HEAD"])
            .await
            .as_ref()
            != Some(name)
        {
            return Err("The prepared checkout no longer uses its intended branch".to_owned());
        }
        let CheckoutPreparationPlan::Git { source_commit, .. } = &plan.plan;
        let commit = self
            .text(
                &plan.destination.path,
                &["rev-parse", "--verify", "HEAD^{commit}"],
            )
            .await
            .ok_or("Prepared checkout HEAD is unavailable; its branch may have been deleted")?;
        if !plan.checkout_created && &commit != source_commit {
            return Err("The destination does not contain the captured source commit; it was not overwritten".into());
        }
        Ok(())
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
        crate::paths::canonical(path).ok()
    }
    async fn valid_root(&self, path: &Path, common: &Path) -> Option<PathBuf> {
        let root = self.text(path, &["rev-parse", "--show-toplevel"]).await?;
        let root = crate::paths::canonical(root).ok()?;
        let path = crate::paths::canonical(path).ok()?;
        if root != path || self.common(&root).await.as_deref() != Some(common) {
            return None;
        }
        Some(root)
    }
}

#[async_trait]
impl SourceControl for GitSourceControl {
    async fn checkpoint(
        &self,
        at: super::PreparationCheckpoint,
        plan: &PreparedCheckout,
    ) -> Result<(), String> {
        if let Some(observer) = &self.observer {
            observer.checkpoint(at, plan).await?;
        }
        Ok(())
    }

    async fn plan_checkout(
        &self,
        id: PreparationId,
        source: &ResolvedWorkspace,
        description: &str,
        channel: &str,
    ) -> Result<PreparedCheckout, String> {
        let repository = source
            .workspace
            .repository
            .as_ref()
            .ok_or("A Repository is required")?;
        let root = match &repository.location {
            RepositoryLocation::Main { root } | RepositoryLocation::Bare { root } => root,
            RepositoryLocation::UnknownMain => {
                return Err("Managed creation requires the main checkout location".to_owned());
            }
        };
        let source_path = source
            .execution_directory
            .as_ref()
            .map(|d| d.path.as_path())
            .unwrap_or(root);
        if self.common(source_path).await.as_ref() != Some(&repository.metadata_directory) {
            return Err("The source checkout no longer belongs to this Repository".to_owned());
        }
        let commit = self
            .text(source_path, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
            .ok_or("A usable local source commit is required; this Repository may be unborn")?;
        let description = portable_description(description);
        // A Channel is part of a portable destination, never a caller supplied path.
        if channel.is_empty()
            || channel == "."
            || channel == ".."
            || channel.ends_with('.')
            || !channel
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err("Channel cannot be used as a managed directory name".to_owned());
        }
        for _ in 0..32 {
            let name = format!(
                "{description}-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            );
            let branch = format!("suru/{name}");
            let destination = root.join(".suru-worktrees").join(channel).join(&name);
            if destination.exists()
                || self
                    .text(
                        source_path,
                        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
                    )
                    .await
                    .is_some()
            {
                continue;
            }
            return Ok(PreparedCheckout {
                id,
                source: ExecutionDirectory {
                    path: source_path.to_owned(),
                },
                repository: repository.clone(),
                destination: ExecutionDirectory { path: destination },
                plan: CheckoutPreparationPlan::Git {
                    branch,
                    source_commit: commit,
                },
                checkout_created: false,
                ready: false,
                intended_session: SessionId::new(),
                admitted_session: None,
            });
        }
        Err("Could not allocate a unique managed Worktree name".to_owned())
    }
    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.inspect_linked_removal(target).await
    }
    async fn remove_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        force: bool,
    ) -> Result<(), String> {
        // The Server just validated these facts under its mutation guard and
        // checked Working after inspection. No newer reading may silently
        // broaden the user's confirmation here.
        if inspection.requires_force() && !force {
            return Err("Worktree contains changes, untracked contents, a lock, or initialized submodules; explicit force confirmation is required".into());
        }
        let root: PathBuf = target.checkout.root.components().collect();
        let root = root
            .to_str()
            .ok_or("Worktree path cannot be passed to Git")?;
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        if force && inspection.lock.is_some() {
            args.push("--force");
        }
        args.extend(["--", root]);
        self.mutate(&target.repository.metadata_directory, &args)
            .await
    }
    async fn prepare_checkout(&self, plan: &PreparedCheckout) -> Result<ResolvedWorkspace, String> {
        let root = match &plan.repository.location {
            RepositoryLocation::Main { root } | RepositoryLocation::Bare { root } => root,
            RepositoryLocation::UnknownMain => {
                return Err("The main checkout location is unknown".to_owned());
            }
        };
        if self.common(root).await.as_ref() != Some(&plan.repository.metadata_directory) {
            return Err("Repository metadata is unavailable or has changed".to_owned());
        }
        let destination = &plan.destination.path;
        // Reject aliases and replacement parents before creating or reusing files.
        let relative = destination
            .strip_prefix(root)
            .map_err(|_| "Managed destination escaped its Repository")?;
        if relative.components().count() != 3
            || relative
                .components()
                .next()
                .is_none_or(|c| c.as_os_str() != ".suru-worktrees")
        {
            return Err("Invalid managed destination".to_owned());
        }
        let mut parent = root.clone();
        for component in relative.components().take(2) {
            parent.push(component);
            match std::fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(
                        "Managed destination parent is not an ordinary directory".to_owned()
                    );
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&parent).map_err(|e| e.to_string())?
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let exclude = plan
            .repository
            .metadata_directory
            .join("info")
            .join("exclude");
        let mut contents = match std::fs::read(&exclude) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        if !contents
            .split(|b| *b == b'\n')
            .any(|line| line.strip_suffix(b"\r").unwrap_or(line) == b"/.suru-worktrees/")
        {
            if !contents.is_empty() && !contents.ends_with(b"\n") {
                contents.push(b'\n');
            }
            contents.extend_from_slice(b"/.suru-worktrees/\n");
            std::fs::create_dir_all(exclude.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&exclude, contents)
                .map_err(|e| format!("Cannot update repository-local exclude: {e}"))?;
        }
        self.claim_branch(plan).await?;
        self.materialize_owned(plan).await?;
        Ok(self.discover(destination).await)
    }
    async fn initialize_checkout(&self, plan: &PreparedCheckout) -> Result<(), String> {
        self.validate_prepared(plan).await?;
        self.mutate(
            &plan.destination.path,
            &["submodule", "update", "--init", "--recursive"],
        )
        .await
        .map_err(|e| {
            format!(
                "Worktree retained at {}. Submodule initialization failed; retry: {e}",
                plan.destination.path.display()
            )
        })
    }
    async fn recover_checkout(
        &self,
        repository: &Repository,
        checkout: &CheckoutAssociation,
    ) -> Result<CheckoutRecovery, String> {
        self.restore_known_checkout(repository, checkout).await
    }
    async fn observe(&self, checkout: &CheckoutAssociation) -> CheckoutSummary {
        self.observe_checkout(checkout, false).await
    }

    /// One `worktree list` per Repository names its Worktrees; nothing here
    /// reads or validates them, because the caller observes each in turn and
    /// that reading is what says whether a root is still this Worktree.
    async fn list_checkouts(&self, repository: &Repository) -> Vec<CheckoutAssociation> {
        let Ok(output) = self
            .command(
                &repository.metadata_directory,
                &["worktree", "list", "--porcelain", "-z"],
            )
            .await
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        parse_worktrees(&output.stdout)
            .into_iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if entry.bare {
                    return None;
                }
                // Separate metadata layouts report the metadata directory as the
                // main Worktree. Only a main root the Repository already names is
                // a root; anything else here is a label, never a working copy.
                let root = if index == 0 {
                    match &repository.location {
                        RepositoryLocation::Main { root } => root.clone(),
                        RepositoryLocation::Bare { .. } | RepositoryLocation::UnknownMain => {
                            return None;
                        }
                    }
                } else {
                    canonical_checkout_path(&entry.root)
                };
                Some(CheckoutAssociation {
                    recovery_revision: None,
                    id: CheckoutId::from_root(&repository.id, &root),
                    repository: repository.id.clone(),
                    root,
                    kind: if index == 0 {
                        CheckoutKind::Main
                    } else {
                        CheckoutKind::Linked
                    },
                })
            })
            .collect()
    }

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
        let path = crate::paths::canonical(directory).unwrap_or_else(|_| directory.to_owned());
        let mut resolved = ResolvedWorkspace::directory(path.clone());
        if !path.is_dir() {
            resolved.execution_status = ExecutionDirectoryStatus::Unavailable {
                reason: "Execution Directory is missing or unreadable".to_owned(),
            };
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
            .and_then(|text| crate::paths::canonical(text.trim_end_matches(['\r', '\n'])).ok())
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
            .and_then(|path| crate::paths::canonical(path).ok());
        let top = self
            .text(&path, &["rev-parse", "--show-toplevel"])
            .await
            .and_then(|path| crate::paths::canonical(path).ok());
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
                    let valid = self.valid_root(&root, &common).await.is_some()
                        && !recovery_in_progress(&root);
                    let association = CheckoutAssociation {
                        // Discovery is already a successful reading. Preserve it
                        // before any Client starts live observation, but never
                        // promote stale Git listing data for an unavailable root.
                        recovery_revision: if valid { entry.revision.clone() } else { None },
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
                    recovery_revision: None,
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
        capabilities.recover_checkout = SourceControlCapability::Available;
        capabilities.remove_checkout = SourceControlCapability::Available;
        capabilities.create_checkout = if matches!(location, RepositoryLocation::UnknownMain) {
            SourceControlCapability::Unsupported {
                reason: "The main checkout location is unknown".to_owned(),
            }
        } else if self
            .text(&directory, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
            .is_none()
        {
            SourceControlCapability::Unsupported {
                reason: "A usable local commit is required; this Repository may be unborn"
                    .to_owned(),
            }
        } else {
            SourceControlCapability::Available
        };

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
            resolved.execution_status = ExecutionDirectoryStatus::RequiresWorkingCopy;
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
            let root = crate::paths::canonical(ancestor).ok()?;
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

fn portable_description(prompt: &str) -> String {
    let mut value = String::new();
    for word in prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if !value.is_empty() {
            value.push('-');
        }
        value.extend(
            word.to_ascii_lowercase()
                .chars()
                .take(40usize.saturating_sub(value.len())),
        );
        if value.len() >= 40 {
            break;
        }
    }
    let value = value.trim_matches('-');
    if value.is_empty() {
        "work".to_owned()
    } else {
        value.to_owned()
    }
}

/// An unfinished recovery must not replace the durable revision it is restoring.
fn recovery_in_progress(root: &Path) -> bool {
    let Ok(pointer) = std::fs::read_to_string(root.join(".git")) else {
        return false;
    };
    let Some(metadata) = pointer
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("gitdir: ")
    else {
        return false;
    };
    let metadata = root.join(metadata);
    metadata.join("suru-recovery").exists()
        || std::fs::read_to_string(metadata.join("locked"))
            .is_ok_and(|lock| lock.starts_with("suru-recovery:"))
}
