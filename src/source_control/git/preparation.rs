//! Git-owned evidence for interrupted mutations. A path and matching revision
//! are insufficient: the atomic branch claim and working-copy metadata must
//! both name this exact persisted intention.
use super::*;
use std::io::Write;

/// What Git holds as proof that a ref, a branch, or a Worktree's lock is this
/// exact persisted intention's. It hashes the intent as serialized, so a
/// change to how any part of it serializes moves the token of every intent
/// already persisted, which then proves nothing it owns.
pub(super) fn token(plan: &PreparedCheckout) -> String {
    let identity = serde_json::to_vec(&(
        &plan.id,
        &plan.repository.id,
        &plan.destination,
        &plan.plan,
        &plan.intended_session,
    ))
    .expect("preparation identity serializes");
    format!("suru-preparation:{}", blake3::hash(&identity).to_hex())
}
impl GitSourceControl {
    /// Whether a lock is backed by both pieces of Git-owned evidence Suru
    /// writes during preparation. This remains verifiable after the intent is
    /// retired: the registration marker and ownership-ref reflog carry the
    /// same opaque token. A raw `suru-preparation:` prefix proves nothing.
    pub(super) async fn owns_preparation_lock(
        &self,
        target: &CheckoutRemovalTarget,
        lock: &str,
    ) -> Result<bool, String> {
        let Some(metadata) = self.recovery_registration(&target.repository, &target.checkout)?
        else {
            return Ok(false);
        };
        let marker = match std::fs::read_to_string(metadata.join("suru-preparation")) {
            Ok(marker) => marker,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        if marker != lock {
            return Ok(false);
        }
        let references = self
            .text(
                &target.repository.metadata_directory,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/suru/preparations",
                ],
            )
            .await
            .unwrap_or_default();
        for reference in references.lines() {
            let message = self
                .text(
                    &target.repository.metadata_directory,
                    &["reflog", "show", "--format=%gs", "-1", reference],
                )
                .await;
            if message.as_deref() == Some(lock) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn record_branch_base(&self, plan: &PreparedCheckout) -> Result<(), String> {
        let CheckoutPreparationPlan::Git {
            branch,
            source_commit,
            source_branch,
        } = &plan.plan;
        let base_key = format!("branch.{branch}.suru-base");
        self.mutate(
            &plan.repository.metadata_directory,
            &["config", "--local", &base_key, source_commit],
        )
        .await?;
        if let Some(source_branch) = source_branch {
            let branch_key = format!("branch.{branch}.suru-base-branch");
            self.mutate(
                &plan.repository.metadata_directory,
                &["config", "--local", &branch_key, source_branch],
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn populate_unfinished_checkout(
        &self,
        destination: &Path,
        metadata: &Path,
    ) -> Result<(), String> {
        if metadata.join("index").exists()
            && !self
                .command(destination, &["diff", "--cached", "--quiet", "HEAD"])
                .await?
                .status
                .success()
        {
            return Err(
                "Incomplete checkout has staged changes; preparation will not overwrite them"
                    .into(),
            );
        }
        // read-tree -m refuses conflicting tracked/untracked user changes. A
        // retry with an already populated index does not reset working files.
        self.mutate(destination, &["read-tree", "-m", "-u", "HEAD"])
            .await?;
        Ok(())
    }
    pub(super) async fn claim_branch(&self, plan: &PreparedCheckout) -> Result<(), String> {
        let CheckoutPreparationPlan::Git {
            branch,
            source_commit,
            ..
        } = &plan.plan;
        let root = &plan.repository.metadata_directory;
        let claim = format!("refs/suru/preparations/{}", plan.id.0.simple());
        let branch_ref = format!("refs/heads/{branch}");
        let expected = token(plan);
        if let Some(commit) = self.text(root, &["rev-parse", "--verify", &claim]).await {
            if commit != *source_commit {
                return Err(
                    "Preparation ownership ref conflicts with its recorded source commit".into(),
                );
            }
            let claim_message = self
                .text(root, &["reflog", "show", "--format=%gs", "-1", &claim])
                .await;
            if claim_message.as_deref() != Some(expected.as_str()) {
                return Err("Preparation ownership ref has conflicting provenance".into());
            }
            if !plan.checkout_created {
                let tip = self
                    .text(root, &["rev-parse", "--verify", &branch_ref])
                    .await;
                if tip.is_none() && self.owned_registration(plan)?.is_none() {
                    let input = format!(
                        "start\nverify {claim} {source_commit}\ncreate {branch_ref} {source_commit}\nprepare\ncommit\n"
                    );
                    let output = self
                        .command_with_input(
                            root,
                            &["update-ref", "--create-reflog", "-m", &expected, "--stdin"],
                            self.mutation_timeout,
                            Some(&input),
                        )
                        .await?;
                    if !output.status.success() {
                        return Err("Missing prepared branch could not be recreated without overwriting conflicting state".into());
                    }
                    self.record_branch_base(plan).await?;
                    return Ok(());
                }
                let creation = self
                    .text(root, &["reflog", "show", "--format=%gs", "-1", &branch_ref])
                    .await;
                if tip.as_ref() != Some(source_commit)
                    || creation.as_deref() != Some(expected.as_str())
                {
                    return Err("Prepared branch is missing or has been replaced or changed; unrelated work was not reset".into());
                }
            }
            self.record_branch_base(plan).await?;
            return Ok(());
        }
        if plan.checkout_created {
            return Err(
                "Preparation ownership evidence is missing; checkout was not changed".into(),
            );
        }
        // Both refs are created or neither is. An external branch winning the
        // name is always a conflict, even when it points at the same commit.
        let input = format!(
            "start\ncreate {branch_ref} {source_commit}\ncreate {claim} {source_commit}\nprepare\ncommit\n"
        );
        let output = self
            .command_with_input(
                root,
                &["update-ref", "--create-reflog", "-m", &expected, "--stdin"],
                self.mutation_timeout,
                Some(&input),
            )
            .await?;
        if !output.status.success() {
            return Err(format!(
                "Cannot claim prepared branch without overwriting existing work: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        self.record_branch_base(plan).await?;
        self.checkpoint(super::super::PreparationCheckpoint::BranchCreated, plan)
            .await
    }

    pub(super) fn owned_registration(
        &self,
        plan: &PreparedCheckout,
    ) -> Result<Option<PathBuf>, String> {
        let directory = plan.repository.metadata_directory.join("worktrees");
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let mut found = None;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            let path = entry.path();
            let Ok(pointer) = std::fs::read_to_string(path.join("gitdir")) else {
                continue;
            };
            if canonical_checkout_path(Path::new(pointer.trim_end_matches(['\r', '\n'])))
                != canonical_checkout_path(&plan.destination.path.join(".git"))
            {
                continue;
            }
            if found.is_some() {
                return Err(
                    "Multiple checkout registrations make preparation ownership ambiguous".into(),
                );
            }
            self.validate_ownership(plan, &path)?;
            found = Some(path);
        }
        Ok(found)
    }
    fn validate_ownership(&self, plan: &PreparedCheckout, metadata: &Path) -> Result<(), String> {
        let expected = token(plan);
        let marker = match std::fs::read_to_string(metadata.join("suru-preparation")) {
            Ok(value) if value == expected => Some(value),
            Ok(_) => return Err("Preparation marker conflicts with the recorded intention".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Preparation marker cannot be read: {e}")),
        };
        let lock = match std::fs::read_to_string(metadata.join("locked")) {
            Ok(value) => Some(value.trim_end_matches(['\r', '\n']).to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if lock.as_ref().is_some_and(|reason| reason != &expected) {
            return Err(
                "The checkout has an external lock; preparation will not override it".into(),
            );
        }
        if marker.as_deref() != Some(expected.as_str())
            && lock.as_deref() != Some(expected.as_str())
        {
            return Err("The destination belongs to no proven preparation; unrelated contents were not overwritten".into());
        }
        Ok(())
    }
    pub(super) async fn materialize_owned(&self, plan: &PreparedCheckout) -> Result<(), String> {
        let destination = &plan.destination.path;
        let destination_text = destination
            .to_str()
            .ok_or("Git destination is not Unicode")?;
        let root = &plan.repository.metadata_directory;
        let expected = token(plan);
        let CheckoutPreparationPlan::Git { branch, .. } = &plan.plan;
        let registered = self.owned_registration(plan)?;
        let exists = match std::fs::symlink_metadata(destination) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err("Prepared destination is not an ordinary directory".into());
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.to_string()),
        };
        if exists && registered.is_none() {
            return Err(
                "Prepared destination is occupied by unrelated contents; it was not overwritten"
                    .into(),
            );
        }
        if !exists {
            if let Some(metadata) = &registered {
                // Target only this proven, absent working copy. Never prune
                // other registrations or delete files at an existing path.
                if metadata.join("locked").exists() {
                    self.mutate(root, &["worktree", "unlock", destination_text])
                        .await?;
                }
                self.mutate(root, &["worktree", "remove", "--force", destination_text])
                    .await?;
            }
            self.mutate(
                root,
                &[
                    "worktree",
                    "add",
                    "--no-checkout",
                    "--lock",
                    "--reason",
                    &expected,
                    destination_text,
                    branch,
                ],
            )
            .await?;
        }
        self.checkpoint(
            super::super::PreparationCheckpoint::RegistrationCreated,
            plan,
        )
        .await?;
        let metadata = self
            .owned_registration(plan)?
            .ok_or("Prepared checkout registration is unavailable")?;
        self.validate_prepared(plan).await?;
        if metadata.join("suru-preparation").exists() {
            if metadata.join("locked").exists() {
                self.mutate(root, &["worktree", "unlock", destination_text])
                    .await?;
            }
            return Ok(());
        }
        self.populate_unfinished_checkout(destination, &metadata)
            .await?;
        let mut marker = tempfile::NamedTempFile::new_in(&metadata).map_err(|e| e.to_string())?;
        marker
            .write_all(expected.as_bytes())
            .map_err(|e| e.to_string())?;
        marker.as_file().sync_all().map_err(|e| e.to_string())?;
        marker
            .persist(metadata.join("suru-preparation"))
            .map_err(|e| e.to_string())?;
        if metadata.join("locked").exists() {
            self.mutate(root, &["worktree", "unlock", destination_text])
                .await?;
        }
        self.checkpoint(super::super::PreparationCheckpoint::CheckoutCreated, plan)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(last: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | last)
    }

    fn intent(source_branch: Option<&str>) -> PreparedCheckout {
        PreparedCheckout {
            id: PreparationId(identity(1)),
            persisted_at: SessionTimestamp(0),
            source: ExecutionDirectory {
                path: "repository".into(),
            },
            repository: Repository {
                id: RepositoryId("git:example".to_owned()),
                system: "git".to_owned(),
                metadata_directory: "repository/.git".into(),
                location: RepositoryLocation::Main {
                    root: "repository".into(),
                },
                availability: SourceControlAvailability::Available,
                capabilities: SourceControlCapabilities::discovery_only(),
            },
            destination: ExecutionDirectory {
                path: "repository/.suru-worktrees/fix-the-cost-indicator".into(),
            },
            plan: CheckoutPreparationPlan::Git {
                branch: "suru/fix-the-cost-indicator".to_owned(),
                source_commit: "90aae04c34c0d70c91c5be38daeb559f911adfc1".to_owned(),
                source_branch: source_branch.map(str::to_owned),
            },
            checkout_created: true,
            ready: false,
            intended_session: SessionId::from_uuid(identity(2)),
            admitted_session: None,
        }
    }

    /// Git holds an intent's token for as long as the intent stands: in its
    /// ownership ref's reflog, its branch's, and its Worktree's lock. So the
    /// token of an intent already persisted must never move, or the intent
    /// can no longer prove what it owns and is neither resumed nor retired.
    /// These are the tokens Git holds for one made from a detached source,
    /// which is also every one made before plans named a source branch, and
    /// for one made from a branch.
    #[test]
    fn a_persisted_intents_token_is_the_one_git_already_holds() {
        assert_eq!(
            token(&intent(None)),
            "suru-preparation:3097d527d5cf5edc6f653949b6c8e0f97082d2b6390494438308748a288070d1"
        );
        assert_eq!(
            token(&intent(Some("main"))),
            "suru-preparation:1a7ac5c6c7b2741bf332ef29ab426b0a950586d2ad6c20487f68f9a8f55572dc"
        );
    }
}
