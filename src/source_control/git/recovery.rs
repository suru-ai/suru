//! Retained checkouts recover from their own association, including external ones.
use super::*;
use std::io::Write;

impl GitSourceControl {
    pub(super) fn recovery_registration(
        &self,
        repository: &Repository,
        checkout: &CheckoutAssociation,
    ) -> Result<Option<PathBuf>, String> {
        let directory = repository.metadata_directory.join("worktrees");
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Cannot read Worktree registrations: {e}")),
        };
        for entry in entries {
            let metadata = entry.map_err(|e| e.to_string())?.path();
            let pointer = std::fs::read_to_string(metadata.join("gitdir"))
                .map_err(|e| format!("Cannot read Worktree registration: {e}"))?;
            let pointer = PathBuf::from(pointer.trim_end_matches(['\r', '\n']));
            if super::canonical_checkout_path(
                pointer.parent().ok_or("Invalid Worktree registration")?,
            ) == super::canonical_checkout_path(&checkout.root)
            {
                return Ok(Some(metadata));
            }
        }
        Ok(None)
    }
    pub(super) async fn restore_known_checkout(
        &self,
        repository: &Repository,
        checkout: &CheckoutAssociation,
    ) -> Result<CheckoutRecovery, String> {
        if checkout.kind != CheckoutKind::Linked || checkout.repository != repository.id {
            return Err("Only this Repository's known linked Worktree can be recovered".to_owned());
        }
        let common = crate::paths::canonical(&repository.metadata_directory)
            .map_err(|_| "Repository metadata is missing or unreadable")?;
        if RepositoryId::from_metadata("git", &common) != repository.id
            || self.common(&common).await.as_ref() != Some(&common)
        {
            return Err("Repository metadata no longer identifies the known Repository".to_owned());
        }
        let token = format!("suru-recovery:{}", checkout.id.0);
        // Git cannot match a missing registration root with a trailing separator.
        let command_root: PathBuf = checkout.root.components().collect();
        let destination = command_root
            .to_str()
            .ok_or("Worktree path cannot be passed to Git")?;
        let mut registration = self.recovery_registration(repository, checkout)?;
        let mut ours = false;
        let mut materialized = false;
        if let Some(metadata) = &registration {
            match std::fs::read_to_string(metadata.join("locked")) {
                Ok(lock) if lock.trim_end() == token => ours = true,
                Ok(_) if !checkout.root.exists() => return Err("Worktree recovery is blocked by an external registration lock; unlock it explicitly before retrying".to_owned()),
                Ok(_) => {},
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(e.to_string()),
            }
            match std::fs::read_to_string(metadata.join("suru-recovery")) {
                Ok(marker) if marker == token => {
                    materialized = true;
                    ours = true;
                }
                Ok(_) => {
                    return Err(
                        "Worktree recovery progress conflicts with the retained checkout identity"
                            .to_owned(),
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        if checkout.root.exists() {
            if std::fs::symlink_metadata(&checkout.root).is_ok_and(|m| m.file_type().is_symlink())
                || self.valid_root(&checkout.root, &common).await.as_ref() != Some(&checkout.root)
            {
                return Err("The original Worktree path is occupied by unrelated contents; nothing was overwritten".to_owned());
            }
            if registration.is_none() {
                return Err("The original path is not the known linked Worktree".to_owned());
            }
            if !ours {
                return Ok(CheckoutRecovery {
                    checkout: checkout.clone(),
                    recreated: false,
                });
            }
        } else {
            // Resolve the retained branch at its current local tip; detached
            // recovery has only its last observed commit, never an invented ref.
            let (revision, detached) = match &checkout.recovery_revision {
                Some(CheckoutRevision::Branch { name, .. }) => {
                    let reference = format!("refs/heads/{name}");
                    self.text(
                        &common,
                        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
                    )
                    .await
                    .ok_or("The retained recovery branch was deleted or has no usable commit")?;
                    (name.clone(), false)
                }
                Some(CheckoutRevision::Detached { commit }) => {
                    self.text(
                        &common,
                        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
                    )
                    .await
                    .ok_or("The retained detached commit is unavailable")?;
                    (commit.clone(), true)
                }
                None => {
                    return Err(
                        "No retained branch or commit is available for Worktree recovery"
                            .to_owned(),
                    );
                }
            };
            if registration.is_some() {
                // Even an unfinished own lock is removed only through Git,
                // and only when this exact root is absent.
                if ours {
                    self.mutate(&common, &["worktree", "unlock", destination])
                        .await?;
                }
                self.mutate(&common, &["worktree", "remove", destination])
                    .await
                    .map_err(|e| format!("Cannot remove this stale Worktree registration: {e}"))?;
            }
            let parent = checkout
                .root
                .parent()
                .ok_or("Worktree has no parent directory")?;
            if canonical_checkout_path(parent) != parent {
                return Err("Original Worktree parent resolves to a different location".to_owned());
            }
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            if crate::paths::canonical(parent).map_err(|e| e.to_string())? != parent {
                return Err("Original Worktree parent resolves to a different location".to_owned());
            }
            let mut args = vec![
                "worktree",
                "add",
                "--lock",
                "--reason",
                &token,
                "--no-checkout",
            ];
            if detached {
                args.push("--detach");
            }
            args.extend(["--", destination, &revision]);
            self.mutate(&common, &args).await.map_err(|e| {
                format!("Cannot recreate the original Worktree; branch may be occupied: {e}")
            })?;
            registration = self.recovery_registration(repository, checkout)?;
            materialized = false;
        }
        let metadata = registration.ok_or("Recovery registration was not created")?;
        let branch = self
            .text(
                &checkout.root,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
            )
            .await;
        let expected = match &checkout.recovery_revision {
            Some(CheckoutRevision::Branch { name, .. }) => branch.as_ref() == Some(name),
            Some(CheckoutRevision::Detached { commit }) => {
                branch.is_none()
                    && self
                        .text(&checkout.root, &["rev-parse", "--verify", "HEAD^{commit}"])
                        .await
                        .as_ref()
                        == Some(commit)
            }
            None => false,
        };
        if !expected {
            return Err("Interrupted recovery no longer has its retained revision; unrelated work was not changed".to_owned());
        }
        if !materialized {
            self.populate_unfinished_checkout(&checkout.root, &metadata)
                .await?;
            let mut marker =
                tempfile::NamedTempFile::new_in(&metadata).map_err(|e| e.to_string())?;
            marker
                .write_all(token.as_bytes())
                .map_err(|e| e.to_string())?;
            marker.as_file().sync_all().map_err(|e| e.to_string())?;
            marker
                .persist(metadata.join("suru-recovery"))
                .map_err(|e| e.to_string())?;
        }
        if metadata.join("locked").exists() {
            let lock =
                std::fs::read_to_string(metadata.join("locked")).map_err(|e| e.to_string())?;
            if lock.trim_end() != token {
                return Err("External Worktree lock blocks completion of recovery".to_owned());
            }
            self.mutate(&common, &["worktree", "unlock", destination])
                .await?;
        }
        self.mutate(
            &checkout.root,
            &["submodule", "update", "--init", "--recursive"],
        )
        .await
        .map_err(|e| {
            format!(
                "Worktree recreated, but submodule initialization failed; retry to continue: {e}"
            )
        })?;
        let reading = self.observe_checkout(checkout, true).await;
        if !matches!(reading.availability, SourceControlAvailability::Available) {
            return Err("Recovered Worktree remains unavailable".to_owned());
        }
        // Keep proof through the last await: cancellation must leave the next
        // caller able to report recreation and invalidate every warm actor.
        std::fs::remove_file(metadata.join("suru-recovery")).map_err(|e| e.to_string())?;
        let mut association = checkout.clone();
        association.recovery_revision = reading.revision;
        Ok(CheckoutRecovery {
            checkout: association,
            recreated: true,
        })
    }
}
