//! Automatic Reclaim of unused Managed Worktrees.

use super::ServerTimings;
use crate::{
    protocol::{
        AutoReclaim, CheckoutAssociation, CheckoutBranchOutcome, CheckoutKind, CheckoutReclaim,
        CheckoutReclaimPhase, CheckoutRemovalTarget, ReclaimOperationId, Repository,
        RepositoryLocation,
    },
    sessions::SessionStore,
    source_control::{PreparationStore, SourceControlService},
};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
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
        let AutoReclaim::AfterDays(days) = self.settings.borrow().settings.worktree.auto_reclaim
        else {
            return;
        };
        for repository in self.source_control.repositories() {
            self.reclaim_repository(&repository, days).await;
        }
    }

    /// One Repository pass, reusable by the later eager-on-delete rule.
    async fn reclaim_repository(&self, repository: &Repository, days: u64) {
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
            if !is_managed(repository, &checkout) {
                continue;
            }
            let activity = self.sessions.checkout_activity(&checkout.id);
            let rule = if activity.affected == 0 {
                ReclaimRule::Orphaned
            } else if idle(activity, days) {
                ReclaimRule::Idle { days }
            } else {
                continue;
            };
            if self
                .preparations
                .for_destination(&checkout.root)
                .is_ok_and(|plans| plans.iter().any(|plan| plan.admitted_session.is_none()))
            {
                // Failed preparations have their own age rule in #352. Their
                // stored intent must keep them out of the immediate orphan rule.
                continue;
            }
            self.reclaim(repository, checkout, rule).await;
        }
    }

    async fn reclaim(
        &self,
        repository: &Repository,
        checkout: CheckoutAssociation,
        rule: ReclaimRule,
    ) {
        let target = CheckoutRemovalTarget {
            repository: repository.clone(),
            checkout: checkout.clone(),
        };
        self.source_control
            .reclaim_candidate_observed(&target)
            .await;
        // Probe in preparation -> Repository order and skip a busy Repository.
        // An unattended pass never queues behind Provider startup while
        // holding the global preparation serial; the next pass retries.
        let Ok(preparation_serial) = self.preparations.serial.try_lock() else {
            return;
        };
        let Some(_guard) = self.source_control.try_mutation_guard(&repository.id) else {
            return;
        };
        if !rule.qualifies(self.sessions.checkout_activity(&checkout.id)) {
            return;
        }
        let preparations = match self.preparations.for_destination(&checkout.root) {
            Ok(preparations) => preparations,
            Err(error) => {
                self.failed(&checkout, rule, &error);
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
                self.failed(&checkout, rule, &error);
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
        let inspection = match self.source_control.inspect_removal(&target).await {
            Ok(inspection) => inspection,
            Err(error) => {
                self.failed(&checkout, rule, &error);
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
                self.failed(&checkout, rule, &error);
                return;
            }
        };
        // Last possible catalog check: inspection and branch analysis may have
        // taken time, and even an idle newly admitted Session blocks orphaning.
        if !rule.qualifies(self.sessions.checkout_activity(&checkout.id)) {
            return;
        }
        let mut recovery = inspection.checkout.clone();
        if intended_outcome == CheckoutBranchOutcome::Deleted {
            recovery.revision = match &recovery.revision {
                Some(crate::protocol::CheckoutRevision::Branch {
                    commit: Some(commit),
                    ..
                }) => Some(crate::protocol::CheckoutRevision::Detached {
                    commit: commit.clone(),
                }),
                _ => {
                    self.failed(
                        &checkout,
                        rule,
                        "A branch without a retained commit cannot be deleted safely",
                    );
                    return;
                }
            };
        }
        if let Err(error) = self.sessions.record_checkout(recovery.clone()) {
            self.failed(
                &checkout,
                rule,
                &format!("Cannot persist recovery facts before removal: {error}"),
            );
            return;
        }
        if !rule.qualifies(self.sessions.checkout_activity(&checkout.id)) {
            return;
        }
        let removing = CheckoutReclaim {
            operation_id: ReclaimOperationId::default(),
            phase: CheckoutReclaimPhase::Removing,
            reason: rule.unavailable_reason(),
        };
        if let Err(error) = self
            .sessions
            .record_reclaimed_checkout(recovery.clone(), removing.clone())
        {
            self.failed(
                &checkout,
                rule,
                &format!("Cannot persist Reclaim state before removal: {error}"),
            );
            return;
        }
        if !rule.qualifies(self.sessions.checkout_activity(&checkout.id)) {
            let _ = self
                .sessions
                .record_execution_checkout(inspection.checkout.clone());
            return;
        }
        match self
            .source_control
            .reclaim_checkout(&target, &inspection, intended_outcome, &preparations)
            .await
        {
            Ok(actual_outcome) => {
                let final_recovery = if actual_outcome == CheckoutBranchOutcome::Deleted {
                    recovery
                } else {
                    inspection.checkout.clone()
                };
                let removed = CheckoutReclaim {
                    phase: CheckoutReclaimPhase::Removed,
                    ..removing
                };
                // Always publish the completed removal boundary. It invalidates
                // Available reads begun while Git still had the Worktree, even
                // when Git produced the branch outcome inspection predicted.
                if let Err(error) = self
                    .sessions
                    .record_reclaimed_checkout(final_recovery, removed)
                {
                    self.failed(
                        &checkout,
                        rule,
                        &format!("Cannot persist final Reclaim state: {error}"),
                    );
                }
                tracing::info!(
                    checkout = %checkout.root.display(),
                    rule = rule.log_name(),
                    branch_outcome = branch_outcome(actual_outcome),
                    "Managed Worktree Reclaimed"
                );
            }
            Err(error) => {
                let _ = self
                    .sessions
                    .record_execution_checkout(inspection.checkout.clone());
                self.failed(&checkout, rule, &error)
            }
        }
    }

    fn failed(&self, checkout: &CheckoutAssociation, rule: ReclaimRule, error: &str) {
        tracing::warn!(
            checkout = %checkout.root.display(),
            rule = rule.log_name(),
            "Managed Worktree Reclaim failed; will retry: {error}"
        );
    }
}

#[derive(Clone, Copy)]
enum ReclaimRule {
    Orphaned,
    Idle { days: u64 },
}

impl ReclaimRule {
    fn qualifies(self, activity: crate::sessions::CheckoutActivity) -> bool {
        match self {
            Self::Orphaned => activity.affected == 0,
            Self::Idle { days } => idle(activity, days),
        }
    }

    fn log_name(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Idle { .. } => "idle",
        }
    }

    fn unavailable_reason(self) -> String {
        match self {
            Self::Orphaned => "Worktree was Reclaimed after its last Session was deleted; prompt a retained Session to recover it".into(),
            Self::Idle { days: 1 } => "Worktree was Reclaimed after 1 day of inactivity; prompt this Session to recover it".into(),
            Self::Idle { days } => format!("Worktree was Reclaimed after {days} days of inactivity; prompt this Session to recover it"),
        }
    }
}

fn idle(activity: crate::sessions::CheckoutActivity, days: u64) -> bool {
    if activity.affected == 0 || activity.working != 0 {
        return false;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let threshold = days.saturating_mul(24 * 60 * 60 * 1_000);
    activity
        .latest_updated_at
        .is_some_and(|updated| updated.0 < now.saturating_sub(threshold))
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
