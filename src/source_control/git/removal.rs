//! Exact, read-only inspection followed only by Git's linked Worktree removal.
use super::*;
impl GitSourceControl {
    async fn is_ancestor(
        &self,
        repository: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, String> {
        Ok(self
            .command(
                repository,
                &["merge-base", "--is-ancestor", ancestor, descendant],
            )
            .await?
            .status
            .success())
    }

    pub(super) async fn linked_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        let CheckoutRevision::Branch {
            name,
            commit: Some(tip),
        } = inspection
            .checkout
            .revision
            .as_ref()
            .ok_or("The selected Worktree has no readable revision for branch retention")?
        else {
            return Ok(CheckoutBranchOutcome::Retained);
        };
        if inspection.checkout.association.id != target.checkout.id {
            return Err("Removal inspection no longer identifies the selected Worktree".into());
        }
        self.branch_outcome(&target.repository.metadata_directory, name, tip)
            .await
    }

    pub(super) async fn branch_outcome(
        &self,
        repository: &Path,
        name: &str,
        tip: &str,
    ) -> Result<CheckoutBranchOutcome, String> {
        let base = match self
            .text(
                repository,
                &["config", "--get", &format!("branch.{name}.suru-base")],
            )
            .await
        {
            Some(base) => base,
            None => return Ok(CheckoutBranchOutcome::Retained),
        };
        let source_branch = match self
            .text(
                repository,
                &[
                    "config",
                    "--get",
                    &format!("branch.{name}.suru-base-branch"),
                ],
            )
            .await
        {
            Some(branch) => branch,
            None => return Ok(CheckoutBranchOutcome::Retained),
        };
        // Invalid or edited provenance is never grounds to remove a branch.
        if !self.is_ancestor(repository, &base, tip).await? {
            return Ok(CheckoutBranchOutcome::Retained);
        }
        let local_source = format!("refs/heads/{source_branch}");
        let local_status = self
            .command(
                repository,
                &["show-ref", "--verify", "--quiet", &local_source],
            )
            .await?
            .status;
        let merged = if local_status.success() {
            self.is_ancestor(repository, tip, &local_source).await?
        } else if local_status.code() == Some(1) {
            // Once the exact local source branch is gone, any locally known
            // remote-tracking ref may prove the work merged. No fetch is made.
            let refs = self
                .text(
                    repository,
                    &["for-each-ref", "--format=%(refname)", "refs/remotes"],
                )
                .await
                .unwrap_or_default();
            let mut merged = false;
            for reference in refs.lines() {
                if self.is_ancestor(repository, tip, reference).await? {
                    merged = true;
                    break;
                }
            }
            merged
        } else {
            // Only show-ref's documented "not found" status proves absence.
            // Invalid provenance and repository read failures retain the branch.
            return Ok(CheckoutBranchOutcome::Retained);
        };
        Ok(if merged {
            CheckoutBranchOutcome::Deleted
        } else {
            CheckoutBranchOutcome::Retained
        })
    }

    pub(super) async fn delete_branch_if_unused(
        &self,
        repository: &Path,
        name: &str,
        expected_tip: &str,
    ) -> Result<CheckoutBranchOutcome, String> {
        let reference = format!("refs/heads/{name}");
        let listing = self
            .command(repository, &["worktree", "list", "--porcelain", "-z"])
            .await?;
        if !listing.status.success() {
            tracing::warn!(
                branch = name,
                "Worktree was removed but branch use could not be rechecked"
            );
            return Ok(CheckoutBranchOutcome::Retained);
        }
        if parse_worktrees(&listing.stdout).iter().any(|entry| {
            matches!(
                &entry.revision,
                Some(CheckoutRevision::Branch { name: used, .. }) if used == name
            )
        }) {
            tracing::warn!(
                branch = name,
                "Worktree was removed but its branch is used by another Worktree"
            );
            return Ok(CheckoutBranchOutcome::Retained);
        }
        if let Err(error) = self
            .mutate(repository, &["update-ref", "-d", &reference, expected_tip])
            .await
        {
            let branch_is_definitely_absent = self
                .command(repository, &["show-ref", "--verify", "--quiet", &reference])
                .await
                .is_ok_and(|output| output.status.code() == Some(1));
            if !branch_is_definitely_absent {
                tracing::warn!(
                    branch = name,
                    "Worktree was removed but its merged branch was retained: {error}"
                );
                return Ok(CheckoutBranchOutcome::Retained);
            }
        }
        let section = format!("branch.{name}");
        if let Err(error) = self
            .mutate(
                repository,
                &["config", "--local", "--remove-section", &section],
            )
            .await
        {
            tracing::warn!(
                branch = name,
                "Deleted branch configuration could not be removed: {error}"
            );
        }
        let branch_is_absent = self
            .command(repository, &["show-ref", "--verify", "--quiet", &reference])
            .await
            .is_ok_and(|output| output.status.code() == Some(1));
        if branch_is_absent {
            Ok(CheckoutBranchOutcome::Deleted)
        } else {
            tracing::warn!(
                branch = name,
                "Worktree was removed but its branch still exists after deletion"
            );
            Ok(CheckoutBranchOutcome::Retained)
        }
    }

    pub(super) async fn inspect_linked_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        let checkout = &target.checkout;
        let repository = &target.repository;
        if checkout.kind != CheckoutKind::Linked
            || checkout.repository != repository.id
            || CheckoutId::from_root(&repository.id, &checkout.root) != checkout.id
        {
            return Err("Only the selected linked Worktree can be removed; the main checkout cannot be removed".into());
        }
        let common = crate::paths::canonical(&repository.metadata_directory)
            .map_err(|_| "Repository metadata is unavailable")?;
        if RepositoryId::from_metadata("git", &common) != repository.id
            || self.common(&common).await.as_ref() != Some(&common)
            || std::fs::symlink_metadata(&checkout.root).is_ok_and(|m| m.file_type().is_symlink())
            || self.valid_root(&checkout.root, &common).await.as_ref() != Some(&checkout.root)
        {
            return Err("The selected Worktree is missing or its Repository identity changed; nothing was removed".into());
        }
        let metadata = self
            .recovery_registration(repository, checkout)?
            .ok_or("Selected path is not a registered linked Worktree")?;
        let lock = match std::fs::read_to_string(metadata.join("locked")) {
            Ok(reason) => Some(reason.trim_end().to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        // Removal already holds the Repository mutation guard. Read through a
        // verified Suru recovery marker here so Reclaim can inspect and finish
        // that abandoned operation; an external or mismatched lock remains in
        // the inspection and still blocks unattended removal.
        let reading = self.observe_checkout(checkout, true).await;
        if reading.availability != SourceControlAvailability::Available {
            return Err("The selected Worktree cannot be read safely for removal".into());
        }
        let output = self
            .command(
                &checkout.root,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let mut tracked = Vec::new();
        let mut untracked = Vec::new();
        let mut ignored = Vec::new();
        let mut entries = output.stdout.split(|b| *b == 0).filter(|s| !s.is_empty());
        while let Some(entry) = entries.next() {
            if entry.len() < 3 {
                return Err("Git returned invalid removal status".into());
            }
            let path = String::from_utf8_lossy(&entry[3..]).into_owned();
            match &entry[..2] {
                b"??" => untracked.push(path),
                b"!!" => ignored.push(path),
                _ => {
                    tracked.push(path);
                    if entry[..2].contains(&b'R') || entry[..2].contains(&b'C') {
                        if let Some(old) = entries.next() {
                            tracked.push(String::from_utf8_lossy(old).into_owned());
                        }
                    }
                }
            }
        }
        let output = self
            .command(&checkout.root, &["submodule", "status", "--recursive"])
            .await
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let initialized_submodules = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.starts_with('-'))
            .map(str::to_owned)
            .collect();
        Ok(CheckoutRemovalInspection {
            checkout: reading,
            tracked,
            untracked,
            ignored,
            lock,
            initialized_submodules,
        })
    }
}
