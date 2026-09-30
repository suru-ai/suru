//! Landing execution-location choice. These are Server readings, never local Git operations.
use crate::protocol::{CheckoutId, CheckoutSummary, ExecutionDirectoryStatus, ResolvedWorkspace};

use super::list_window::ListWindow;

#[derive(Clone, Debug, Default)]
pub(super) struct WorktreePicker {
    pub(super) removal: Option<crate::protocol::CheckoutRemovalPreview>,
    pub(super) removal_request: Option<uuid::Uuid>,
    pub(super) open: bool,
    pub(super) loading: bool,
    pub(super) selected: usize,
    pub(super) context: Option<ResolvedWorkspace>,
    pub(super) error: Option<String>,
    pub(super) window: ListWindow,
}

/// What a row stands for. The Worktree the reader is already in is one of the
/// Worktrees rather than an entry of its own, so choosing it is choosing a
/// Checkout like any other and the Landing decides it has nowhere to move to.
pub(super) enum WorktreeChoice {
    Checkout(Box<CheckoutSummary>),
    New,
}

impl WorktreePicker {
    pub(super) fn open(&mut self) {
        *self = Self {
            open: true,
            loading: true,
            ..Default::default()
        };
        self.window.open();
    }
    pub(super) fn close(&mut self) {
        *self = Self::default();
    }
    pub(super) fn load(&mut self, context: ResolvedWorkspace) {
        self.context = Some(context);
        self.loading = false;
        self.error = None;
    }
    pub(super) fn fail(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }
    pub(super) fn move_by(&mut self, delta: isize) {
        if self.loading || self.removal.is_some() {
            return;
        }
        let count = self.rows();
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.window.reveal();
    }
    /// New Worktree first, then one row per Worktree of the Repository.
    fn rows(&self) -> usize {
        self.context
            .as_ref()
            .map_or(1, |context| context.checkouts.len() + 1)
    }
    pub(super) fn choice(&self) -> Option<WorktreeChoice> {
        if self.loading {
            return None;
        }
        let context = self.context.as_ref()?;
        if self.selected == 0 {
            Some(WorktreeChoice::New)
        } else {
            context
                .checkouts
                .get(self.selected - 1)
                .map(|checkout| WorktreeChoice::Checkout(Box::new(checkout.clone())))
        }
    }
    /// The Worktree the next Session would already work in, which its row
    /// marks and choosing again means staying put.
    pub(super) fn current_checkout(&self) -> Option<&CheckoutId> {
        self.context
            .as_ref()?
            .checkout
            .as_ref()
            .map(|checkout| &checkout.id)
    }
    pub(super) fn current_status(&self) -> &str {
        match self
            .context
            .as_ref()
            .map(|context| &context.execution_status)
        {
            Some(ExecutionDirectoryStatus::Unavailable { .. }) => " · unavailable",
            Some(ExecutionDirectoryStatus::RequiresWorkingCopy) => " · choose a working copy",
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorktreeChoice, WorktreePicker};
    use crate::protocol::{
        CheckoutAssociation, CheckoutId, CheckoutKind, CheckoutSummary, ExecutionDirectoryStatus,
        RepositoryId, ResolvedWorkspace, SourceControlAvailability, Workspace,
    };
    use std::path::{Path, PathBuf};

    /// Windows reads `/main` as a relative path, so every fixture root is
    /// spelled for the platform the test runs on.
    fn root(name: &str) -> PathBuf {
        if cfg!(windows) {
            Path::new(r"C:\repository").join(name)
        } else {
            Path::new("/repository").join(name)
        }
    }

    fn loaded() -> WorktreePicker {
        let repository = RepositoryId::from_metadata("git", &root("main"));
        let checkouts = ["main", "linked"]
            .into_iter()
            .map(|name| CheckoutSummary {
                association: CheckoutAssociation {
                    recovery_revision: None,
                    reclaim: None,
                    id: CheckoutId::from_root(&repository, &root(name)),
                    repository: repository.clone(),
                    root: root(name),
                    kind: if name == "main" {
                        CheckoutKind::Main
                    } else {
                        CheckoutKind::Linked
                    },
                },
                revision: None,
                availability: SourceControlAvailability::Available,
            })
            .collect::<Vec<_>>();
        let mut picker = WorktreePicker::default();
        picker.open();
        picker.load(ResolvedWorkspace {
            execution_status: ExecutionDirectoryStatus::Available,
            workspace: Workspace::directory(root("main")),
            execution_directory: None,
            checkout: Some(checkouts[1].association.clone()),
            checkouts,
        });
        picker
    }

    #[test]
    fn the_new_worktree_row_leads_and_is_where_an_opened_picker_stands() {
        let picker = loaded();
        assert_eq!(picker.selected, 0);
        assert!(matches!(picker.choice(), Some(WorktreeChoice::New)));
    }

    #[test]
    fn moving_walks_every_worktree_and_wraps_around_the_new_worktree_row() {
        let mut picker = loaded();
        picker.move_by(1);
        let Some(WorktreeChoice::Checkout(checkout)) = picker.choice() else {
            panic!("the first Worktree follows the New Worktree row")
        };
        assert_eq!(checkout.association.root, root("main"));
        picker.move_by(1);
        let Some(WorktreeChoice::Checkout(checkout)) = picker.choice() else {
            panic!("the second Worktree follows the first")
        };
        assert_eq!(checkout.association.root, root("linked"));
        picker.move_by(1);
        assert_eq!(picker.selected, 0);
        picker.move_by(-1);
        assert_eq!(
            picker.selected, 2,
            "moving back wraps onto the last Worktree"
        );
    }

    #[test]
    fn the_current_worktree_is_named_by_the_resolution_rather_than_by_position() {
        let picker = loaded();
        let current = picker.current_checkout().expect("a resolved Worktree");
        assert_eq!(
            *current,
            picker.context.as_ref().unwrap().checkouts[1].association.id
        );
    }

    #[test]
    fn a_loading_picker_offers_no_choice_and_does_not_move() {
        let mut picker = WorktreePicker::default();
        picker.open();
        picker.move_by(1);
        assert_eq!(picker.selected, 0);
        assert!(picker.choice().is_none());
    }
}
