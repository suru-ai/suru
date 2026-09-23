//! Which branch names are free, and renaming the branch Suru created for a
//! Managed Worktree to one a Title derivation proposed.
use super::*;
use crate::source_control::{BranchRename, CreatedBranch, naming};

impl GitSourceControl {
    /// Every local branch's short name, read in one listing so a whole run of
    /// numbered names is judged against the same state.
    pub(super) async fn branch_names(&self, directory: &Path) -> Result<Vec<String>, String> {
        let output = self
            .command(
                directory,
                &["for-each-ref", "--format=%(refname)", "refs/heads/"],
            )
            .await?;
        if !output.status.success() {
            return Err(format!(
                "Git could not list branches: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|reference| reference.strip_prefix("refs/heads/"))
            .map(str::to_owned)
            .collect())
    }

    /// Rename a created branch whose Worktree is still on it. See
    /// [`crate::source_control::SourceControl::rename_branch`].
    pub(super) async fn rename_created_branch(
        &self,
        created: &CreatedBranch,
        proposal: &str,
    ) -> Result<BranchRename, String> {
        let root = &created.checkout.root;
        let metadata = &created.repository.metadata_directory;
        if self.valid_root(root, metadata).await.as_ref() != Some(root) {
            return Err(
                "The Worktree is missing or no longer belongs to its Repository".to_owned(),
            );
        }
        let head = self.text(root, &["symbolic-ref", "--quiet", "HEAD"]).await;
        if head.as_deref() != Some(format!("refs/heads/{}", created.branch).as_str()) {
            return Err("The Worktree is no longer on the branch Suru created for it".to_owned());
        }
        // The branch being renamed is the one name that never stands in its
        // own way.
        let others = self
            .branch_names(root)
            .await?
            .into_iter()
            .filter(|name| name != &created.branch)
            .collect::<Vec<_>>();
        for name in naming::numbered(proposal) {
            let branch = format!("{}{name}", naming::BRANCH_PREFIX);
            if branch == created.branch {
                return Ok(BranchRename::Unchanged);
            }
            if !branch_available(&others, &branch) {
                continue;
            }
            // Git moves the branch's reflog and its `branch.<name>.*`
            // configuration with it, Reclaim's recorded base included, and
            // refuses rather than overwrites a branch that appeared since the
            // listing.
            self.mutate(root, &["branch", "-m", &created.branch, &branch])
                .await?;
            return Ok(BranchRename::Renamed { branch });
        }
        Err("Every numbered form of the proposed branch name is taken".to_owned())
    }
}

/// Whether `branch` may be created beside the `existing` branches. Git refuses
/// a branch whose name another uses as a directory (`suru/fix` beside
/// `suru/fix/old`) or that needs an existing branch to be one (`suru/fix/old`
/// beside `suru/fix`), and on a case-insensitive filesystem a loose ref that
/// differs only in case is the same file. All of them count as taken on every
/// platform, so a name never depends on where its Repository happens to live.
pub(super) fn branch_available(existing: &[String], branch: &str) -> bool {
    let wanted = branch.to_lowercase();
    !existing.iter().any(|name| {
        let name = name.to_lowercase();
        let nested = |outer: &str, inner: &str| {
            inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/'))
        };
        name == wanted || nested(&wanted, &name) || nested(&name, &wanted)
    })
}

#[cfg(test)]
mod tests {
    use super::branch_available;

    #[test]
    fn a_name_is_taken_by_its_case_variants_and_by_directory_conflicts() {
        let existing = [
            "main".to_owned(),
            "suru/Fix-Flicker".to_owned(),
            "suru/ship-picker/old".to_owned(),
            "suru/notes".to_owned(),
        ];
        for (branch, available) in [
            ("suru/fix-flicker", false),
            ("SURU/FIX-FLICKER", false),
            ("suru/ship-picker", false),
            ("suru/notes/today", false),
            ("MAIN", false),
            ("suru/fix-flicker-2", true),
            ("suru/ship", true),
            ("suru/ship-picker-2", true),
            ("suru/note", true),
        ] {
            assert_eq!(branch_available(&existing, branch), available, "{branch}");
        }
    }
}
