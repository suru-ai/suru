//! Automatic Reclaim of unused Managed Worktrees.

use super::ServerTimings;
use crate::{
    protocol::{
        AutoReclaim, CheckoutAssociation, CheckoutBranchOutcome, CheckoutKind,
        CheckoutRemovalTarget, Repository, RepositoryLocation,
    },
    sessions::SessionStore,
    source_control::{PreparationStore, SourceControlService},
};
use std::{path::Path, time::Duration};
use tokio::sync::watch;

pub(super) fn spawn(
    sessions: SessionStore,
    source_control: SourceControlService,
    preparations: PreparationStore,
    settings: watch::Receiver<crate::protocol::SettingsSnapshot>,
    mut workspace_discovery: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
    timings: ServerTimings,
) {
    tokio::spawn(async move {
        while !*workspace_discovery.borrow() {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    let _ = changed;
                    return;
                }
                changed = workspace_discovery.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
        if wait_or_shutdown(timings.worktree_reclaim_startup_delay, &mut shutdown).await {
            return;
        }
        let reclaimer = Reclaimer {
            sessions,
            source_control,
            preparations,
            settings,
        };
        if run_pass_or_shutdown(&reclaimer, &mut shutdown).await {
            return;
        }
        let interval = timings
            .worktree_reclaim_interval
            .max(Duration::from_millis(1));
        loop {
            if wait_or_shutdown(interval, &mut shutdown).await {
                return;
            }
            if run_pass_or_shutdown(&reclaimer, &mut shutdown).await {
                return;
            }
        }
    });
}

async fn run_pass_or_shutdown(reclaimer: &Reclaimer, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            let _ = changed;
            true
        }
        _ = reclaimer.pass() => false,
    }
}

async fn wait_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            let _ = changed;
            true
        }
        _ = tokio::time::sleep(duration) => false,
    }
}

struct Reclaimer {
    sessions: SessionStore,
    source_control: SourceControlService,
    preparations: PreparationStore,
    settings: watch::Receiver<crate::protocol::SettingsSnapshot>,
}

impl Reclaimer {
    async fn pass(&self) {
        if matches!(
            self.settings.borrow().settings.worktree.auto_reclaim,
            AutoReclaim::Off
        ) {
            return;
        }
        for repository in self.source_control.repositories() {
            self.reclaim_orphans(&repository).await;
        }
    }

    /// One Repository pass, reusable by the later eager-on-delete rule.
    async fn reclaim_orphans(&self, repository: &Repository) {
        let checkouts = match self.source_control.list_checkouts(repository).await {
            Ok(checkouts) => checkouts,
            Err(error) => {
                tracing::warn!(
                    repository = %repository.presentation_path().display(),
                    rule = "orphaned",
                    "Managed Worktree Reclaim could not list Repository Worktrees; will retry: {error}"
                );
                return;
            }
        };
        for checkout in checkouts {
            if !is_managed(repository, &checkout)
                || self.sessions.checkout_references(&checkout.id).0 != 0
            {
                continue;
            }
            if self
                .preparations
                .for_destination(&checkout.root)
                .is_ok_and(|plans| plans.iter().any(|plan| plan.admitted_session.is_none()))
            {
                // Failed preparations have their own age rule in #352. Their
                // stored intent must keep them out of the immediate orphan rule.
                continue;
            }
            self.reclaim_orphan(repository, checkout).await;
        }
    }

    async fn reclaim_orphan(&self, repository: &Repository, checkout: CheckoutAssociation) {
        // Probe in preparation -> Repository order and skip a busy Repository.
        // An unattended pass never queues behind Provider startup while
        // holding the global preparation serial; the next pass retries.
        let Ok(preparation_serial) = self.preparations.serial.try_lock() else {
            return;
        };
        let Some(_guard) = self.source_control.try_mutation_guard(&repository.id) else {
            return;
        };
        if self.sessions.checkout_references(&checkout.id).0 != 0 {
            return;
        }
        let preparations = match self.preparations.for_destination(&checkout.root) {
            Ok(preparations) => preparations,
            Err(error) => {
                self.failed(&checkout, &error);
                return;
            }
        };
        if preparations
            .iter()
            .any(|plan| plan.admitted_session.is_none())
        {
            return;
        }
        // Admission should already have retired these intents (#348). Finish
        // any durable remainder while both race gates are held, before letting
        // a retry load it as though the deleted Session could still rejoin.
        for preparation in &preparations {
            if let Err(error) = self.preparations.retire(preparation.id) {
                self.failed(&checkout, &error);
                return;
            }
            if let Err(error) = self.preparations.finish_retirement(preparation.id) {
                tracing::warn!(
                    checkout = %checkout.root.display(),
                    preparation = %preparation.id.0,
                    "Retired Worktree preparation cleanup will retry after restart: {error}"
                );
            }
        }
        drop(preparation_serial);
        let target = CheckoutRemovalTarget {
            repository: repository.clone(),
            checkout: checkout.clone(),
        };
        let inspection = match self.source_control.inspect_removal(&target).await {
            Ok(inspection) => inspection,
            Err(error) => {
                self.failed(&checkout, &error);
                return;
            }
        };
        if !inspection.tracked.is_empty()
            || !inspection.untracked.is_empty()
            || !inspection.initialized_submodules.is_empty()
        {
            return;
        }
        let intended_outcome = match self
            .source_control
            .removal_branch_outcome(&target, &inspection)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.failed(&checkout, &error);
                return;
            }
        };
        // Last possible catalog check: inspection and branch analysis may have
        // taken time, and even an idle newly admitted Session blocks orphaning.
        if self.sessions.checkout_references(&checkout.id).0 != 0 {
            return;
        }
        match self
            .source_control
            .reclaim_checkout(&target, &inspection, intended_outcome, &preparations)
            .await
        {
            Ok(actual_outcome) => {
                tracing::info!(
                    checkout = %checkout.root.display(),
                    rule = "orphaned",
                    branch_outcome = branch_outcome(actual_outcome),
                    "Managed Worktree Reclaimed"
                );
            }
            Err(error) => self.failed(&checkout, &error),
        }
    }

    fn failed(&self, checkout: &CheckoutAssociation, error: &str) {
        tracing::warn!(
            checkout = %checkout.root.display(),
            rule = "orphaned",
            "Managed Worktree Reclaim failed; will retry: {error}"
        );
    }
}

fn branch_outcome(outcome: CheckoutBranchOutcome) -> &'static str {
    match outcome {
        CheckoutBranchOutcome::Deleted => "deleted",
        CheckoutBranchOutcome::Retained => "retained",
    }
}

fn is_managed(repository: &Repository, checkout: &CheckoutAssociation) -> bool {
    if checkout.kind != CheckoutKind::Linked || checkout.repository != repository.id {
        return false;
    }
    let root = match &repository.location {
        RepositoryLocation::Main { root } | RepositoryLocation::Bare { root } => root,
        RepositoryLocation::UnknownMain => return false,
    };
    let Ok(relative) = checkout.root.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components();
    components.next().is_some_and(|component| {
        component.as_os_str() == crate::protocol::MANAGED_WORKTREE_DIRECTORY
    }) && components.next().is_some()
        && components.next().is_none()
        && ordinary_directory(&checkout.root)
}

fn ordinary_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}
