//! Exact, read-only inspection followed only by Git's linked Worktree removal.
use super::*;
impl GitSourceControl {
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
        let reading = self.observe(checkout).await;
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
