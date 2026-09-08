//! Landing execution-location choice. These are Server readings, never local Git operations.
use crate::protocol::{CheckoutSummary, ExecutionDirectoryStatus, ResolvedWorkspace};

#[derive(Clone, Debug, Default)]
pub(super) struct WorktreePicker {
    pub(super) open: bool,
    pub(super) loading: bool,
    pub(super) selected: usize,
    pub(super) context: Option<ResolvedWorkspace>,
    pub(super) directory: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) enum WorktreeChoice {
    Current,
    Checkout(CheckoutSummary),
    Directory,
    New,
}

impl WorktreePicker {
    pub(super) fn open(&mut self) {
        *self = Self {
            open: true,
            loading: true,
            ..Default::default()
        };
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
        if self.loading || self.directory.is_some() {
            return;
        }
        let count = self
            .context
            .as_ref()
            .map_or(3, |context| context.checkouts.len() + 3);
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
    }
    pub(super) fn choice(&self) -> Option<WorktreeChoice> {
        if self.loading {
            return None;
        }
        let context = self.context.as_ref()?;
        if self.selected == 0 {
            Some(WorktreeChoice::Current)
        } else if self.selected <= context.checkouts.len() {
            Some(WorktreeChoice::Checkout(
                context.checkouts[self.selected - 1].clone(),
            ))
        } else if self.selected == context.checkouts.len() + 1 {
            Some(WorktreeChoice::Directory)
        } else {
            Some(WorktreeChoice::New)
        }
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
