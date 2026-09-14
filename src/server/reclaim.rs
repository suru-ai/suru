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
    collections::{HashMap, HashSet},
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

/// Run the orphan population for the one Repository a successful Session
/// deletion named. This task is deliberately detached from the DELETE
/// response: Git refusal, a busy guard, and shutdown all leave a later sweep
/// to retry without changing the completed Session operation.
pub(super) fn spawn_orphans(
    repository: Repository,
    sessions: SessionStore,
    source_control: SourceControlService,
    preparations: PreparationStore,
    settings: watch::Receiver<crate::protocol::SettingsSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        if *shutdown.borrow() {
            return;
        }
        let reclaimer = Reclaimer {
            sessions,
            source_control,
            preparations,
            settings,
        };
        tokio::select! {
            biased;
            _ = shutdown.changed() => {}
            _ = reclaimer.orphans(&repository) => {}
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
        let preparations = match self.preparations.all() {
            Ok(preparations) => preparations,
            Err(error) => {
                tracing::warn!(
                    "Managed Worktree Reclaim could not read preparation intents; will retry: {error}"
                );
                return;
            }
        };
        let mut repositories = self
            .source_control
            .repositories()
            .into_iter()
            .map(|repository| (repository.id.clone(), repository))
            .collect::<HashMap<_, _>>();
        for preparation in &preparations {
            repositories
                .entry(preparation.repository.id.clone())
                .or_insert_with(|| preparation.repository.clone());
        }
        for repository in repositories.values() {
            let repository_preparations = preparations
                .iter()
                .filter(|preparation| preparation.repository.id == repository.id)
                .cloned()
                .collect::<Vec<_>>();
            self.reclaim_repository(
                repository,
                days,
                &repository_preparations,
                ReclaimPopulation::All,
            )
            .await;
        }
    }

    async fn orphans(&self, repository: &Repository) {
        let AutoReclaim::AfterDays(days) = self.settings.borrow().settings.worktree.auto_reclaim
        else {
            return;
        };
        let preparations = match self.preparations.all() {
            Ok(preparations) => preparations
                .into_iter()
                .filter(|preparation| preparation.repository.id == repository.id)
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(
                    repository = %repository.presentation_path().display(),
                    rule = "orphaned",
                    "Managed Worktree Reclaim could not read preparation intents; will retry: {error}"
                );
                return;
            }
        };
        self.reclaim_repository(repository, days, &preparations, ReclaimPopulation::Orphans)
            .await;
    }

    /// One Repository pass shared by the scheduled and eager paths. The eager
    /// path selects only orphans, so deleting a Session never accelerates an
    /// idle or failed-preparation threshold.
    async fn reclaim_repository(
        &self,
        repository: &Repository,
        days: u64,
        preparations: &[crate::protocol::PreparedCheckout],
        population: ReclaimPopulation,
    ) {
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
        let listed = checkouts
            .iter()
            .map(|checkout| checkout.root.clone())
            .collect::<HashSet<_>>();
        for checkout in checkouts {
            if !is_managed(repository, &checkout) {
                continue;
            }
            let unfinished = preparations
                .iter()
                .filter(|plan| {
                    plan.destination.path == checkout.root && plan.admitted_session.is_none()
                })
                .collect::<Vec<_>>();
            let activity = self.sessions.checkout_activity(&checkout.id);
            let rule = if !unfinished.is_empty() {
                if population == ReclaimPopulation::All
                    && unfinished.iter().all(|plan| old(plan, days))
                {
                    ReclaimRule::FailedPreparation { days }
                } else {
                    continue;
                }
            } else if activity.affected == 0 {
                ReclaimRule::Orphaned
            } else if population == ReclaimPopulation::All && idle(activity, days) {
                ReclaimRule::Idle { days }
            } else {
                continue;
            };
            self.reclaim(repository, checkout, rule).await;
        }
        if population == ReclaimPopulation::Orphans {
            return;
        }
        for preparation in preparations.iter().filter(|plan| {
            plan.admitted_session.is_none()
                && old(plan, days)
                && !listed.contains(&plan.destination.path)
                && !plan.destination.path.exists()
        }) {
            self.finish_failed_preparation_remainder(repository, preparation, days)
                .await;
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
        let mut preparations = match self.preparations.for_destination(&checkout.root) {
            Ok(preparations) => preparations,
            Err(error) => {
                self.failed(&checkout, rule, &error);
                return;
            }
        };
        if rule.is_failed_preparation() {
            if preparations.is_empty()
                || preparations
                    .iter()
                    .any(|plan| plan.admitted_session.is_some() || !old(plan, rule.days()))
            {
                return;
            }
            // The CheckoutCreated observer may fail before the request handler
            // records this fact. Persist it once Git lists the Worktree so a
            // post-removal retry never mistakes the remainder for branch-only
            // preparation and broadens the branch fate already decided here.
            for preparation in &mut preparations {
                if !preparation.checkout_created {
                    preparation.checkout_created = true;
                    if let Err(error) = self.preparations.save(preparation) {
                        self.failed(&checkout, rule, &error);
                        return;
                    }
                }
            }
        } else if preparations
            .iter()
            .any(|plan| plan.admitted_session.is_none())
        {
            return;
        }
        // Admission should already have retired these intents (#348). Finish
        // any durable remainder while both race gates are held, before letting
        // a retry load it as though the deleted Session could still rejoin.
        if !rule.is_failed_preparation() {
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
        }
        let preparation_serial = if rule.is_failed_preparation() {
            Some(preparation_serial)
        } else {
            drop(preparation_serial);
            None
        };
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
                if rule.is_failed_preparation() {
                    for preparation in &preparations {
                        if let Err(error) = self
                            .finish_failed_preparation(preparation, &checkout, rule, false)
                            .await
                        {
                            self.failed(&checkout, rule, &error);
                            break;
                        }
                    }
                }
                drop(preparation_serial);
            }
            Err(error) => {
                let _ = self
                    .sessions
                    .record_execution_checkout(inspection.checkout.clone());
                self.failed(&checkout, rule, &error)
            }
        }
    }

    async fn finish_failed_preparation(
        &self,
        preparation: &crate::protocol::PreparedCheckout,
        checkout: &CheckoutAssociation,
        rule: ReclaimRule,
        retire_branch: bool,
    ) -> Result<CheckoutBranchOutcome, String> {
        let branch_outcome = self
            .source_control
            .retire_preparation(preparation, retire_branch)
            .await?;
        let withheld_prompt = self
            .sessions
            .preparation_prompt_first_line(preparation.intended_session)
            .await?;
        if let Some(first_line) = &withheld_prompt {
            tracing::info!(
                checkout = %checkout.root.display(),
                preparation = %preparation.id.0,
                rule = rule.log_name(),
                prompt_first_line = first_line,
                "Failed Worktree preparation Prompt withheld by Reclaim"
            );
        }
        self.sessions
            .retire_preparation_prompt(preparation.intended_session)
            .await?;
        self.preparations.retire(preparation.id)?;
        self.preparations.finish_retirement(preparation.id)?;
        tracing::info!(
            checkout = %checkout.root.display(),
            preparation = %preparation.id.0,
            rule = rule.log_name(),
            "Failed Worktree preparation intent retired"
        );
        Ok(branch_outcome)
    }

    async fn finish_failed_preparation_remainder(
        &self,
        repository: &Repository,
        preparation: &crate::protocol::PreparedCheckout,
        days: u64,
    ) {
        let Ok(_serial) = self.preparations.serial.try_lock() else {
            return;
        };
        let Some(_guard) = self.source_control.try_mutation_guard(&repository.id) else {
            return;
        };
        let current = match self.preparations.load(preparation.id) {
            Ok(Some(current)) => current,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(preparation = %preparation.id.0, "Failed Worktree preparation Reclaim could not reload intent; will retry: {error}");
                return;
            }
        };
        if current.admitted_session.is_some()
            || !old(&current, days)
            || current.destination.path.exists()
            || current.repository.id != repository.id
        {
            return;
        }
        let checkout_id =
            crate::protocol::CheckoutId::from_root(&repository.id, &current.destination.path);
        if self.sessions.checkout_activity(&checkout_id).working != 0 {
            return;
        }
        let listed = match self.source_control.list_checkouts(repository).await {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(
                    checkout = %current.destination.path.display(),
                    rule = "failed preparation",
                    "Managed Worktree Reclaim could not confirm removed preparation; will retry: {error}"
                );
                return;
            }
        };
        if listed
            .iter()
            .any(|checkout| checkout.root == current.destination.path)
        {
            return;
        }
        let checkout = CheckoutAssociation {
            id: checkout_id,
            repository: repository.id.clone(),
            root: current.destination.path.clone(),
            kind: CheckoutKind::Linked,
            recovery_revision: None,
            reclaim: None,
        };
        let rule = ReclaimRule::FailedPreparation { days };
        let retire_branch = !current.checkout_created;
        match self
            .finish_failed_preparation(&current, &checkout, rule, retire_branch)
            .await
        {
            Ok(outcome) if retire_branch => tracing::info!(
                checkout = %checkout.root.display(),
                rule = rule.log_name(),
                branch_outcome = branch_outcome(outcome),
                "Managed Worktree Reclaimed"
            ),
            Ok(_) => {}
            Err(error) => self.failed(&checkout, rule, &error),
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReclaimPopulation {
    All,
    Orphans,
}

#[derive(Clone, Copy)]
enum ReclaimRule {
    Orphaned,
    Idle { days: u64 },
    FailedPreparation { days: u64 },
}

impl ReclaimRule {
    fn qualifies(self, activity: crate::sessions::CheckoutActivity) -> bool {
        match self {
            Self::Orphaned => activity.affected == 0,
            Self::Idle { days } => idle(activity, days),
            Self::FailedPreparation { .. } => activity.working == 0,
        }
    }

    fn log_name(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Idle { .. } => "idle",
            Self::FailedPreparation { .. } => "failed preparation",
        }
    }

    fn unavailable_reason(self) -> String {
        match self {
            Self::Orphaned => "Worktree was Reclaimed after its last Session was deleted; prompt a retained Session to recover it".into(),
            Self::Idle { days: 1 } => "Worktree was Reclaimed after 1 day of inactivity; prompt this Session to recover it".into(),
            Self::Idle { days } => format!("Worktree was Reclaimed after {days} days of inactivity; prompt this Session to recover it"),
            Self::FailedPreparation { days: 1 } => "Worktree was Reclaimed after its preparation failed 1 day ago".into(),
            Self::FailedPreparation { days } => format!("Worktree was Reclaimed after its preparation failed {days} days ago"),
        }
    }

    fn is_failed_preparation(self) -> bool {
        matches!(self, Self::FailedPreparation { .. })
    }

    fn days(self) -> u64 {
        match self {
            Self::Idle { days } | Self::FailedPreparation { days } => days,
            Self::Orphaned => 0,
        }
    }
}

fn old(preparation: &crate::protocol::PreparedCheckout, days: u64) -> bool {
    let Some(persisted_at) = preparation.persisted_at else {
        return true;
    };
    let threshold = days.saturating_mul(24 * 60 * 60 * 1_000);
    persisted_at.0
        < crate::protocol::SessionTimestamp::now()
            .0
            .saturating_sub(threshold)
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
