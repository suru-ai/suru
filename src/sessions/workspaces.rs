use std::path::PathBuf;

use super::{SessionStore, SessionStoreState};
use crate::{
    protocol::{SessionChange, SessionId, Workspace, WorkspaceId},
    source_control::SourceControlService,
    storage::{StoredWorkspace, WorkspaceProvenance},
};

impl SessionStoreState {
    /// Dresses `workspace` in what this store holds for it (see
    /// [`super::dress_workspace`]).
    pub(super) fn dress_workspace(&self, workspace: &mut Workspace) {
        super::dress_workspace(
            &self.workspaces,
            self.sidekick_workspace.as_ref(),
            workspace,
        );
    }
}

impl SessionStore {
    /// Dresses a Workspace resolved outside this store — one no Session has
    /// been begun in yet — as every copy this store hands out is dressed.
    pub(crate) fn dress_workspace(&self, workspace: &mut Workspace) {
        self.state.lock().unwrap().dress_workspace(workspace);
    }

    /// Whether `id` names the Sidekick Workspace, whose Icon and Description
    /// are never derived.
    pub(crate) fn is_sidekick_workspace(&self, id: &WorkspaceId) -> bool {
        self.state
            .lock()
            .unwrap()
            .sidekick_workspace
            .as_ref()
            .is_some_and(|sidekick| sidekick.is_named_by(id))
    }

    pub(crate) fn known_workspace(&self, id: &WorkspaceId) -> Option<Workspace> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .values()
            .find(|record| &record.summary.session.workspace.id == id)
            .map(|record| record.summary.session.workspace.clone())
    }
    /// Every Workspace this server knows, as its Workspace Registry lists
    /// them (ADR 0051): first those the Sessions a listing holds work in —
    /// every top-level Session's that says, readable or not, as
    /// [`Self::list`] lists them — most recently worked in first; then the
    /// rest, by path. Each is given as its entry presents it — its identity
    /// and the path it was last presented by — dressed in what the entry
    /// carries, with nothing of its Repository, which only a resolution
    /// reads; the Sessions only order them. An entry that says nowhere,
    /// having landed before rows kept a path, cannot be named and is left
    /// out until a Session working there presents it.
    pub(crate) fn listed_workspaces(&self) -> Vec<Workspace> {
        let worked = self
            .list(None)
            .iter()
            .filter_map(crate::protocol::SessionListItem::workspace)
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let recency = |workspace: &Workspace| {
            worked
                .iter()
                .position(|id| id == &workspace.id)
                .unwrap_or(usize::MAX)
        };
        let state = self.state.lock().unwrap();
        let mut workspaces = state
            .workspaces
            .iter()
            .filter_map(|(id, stored)| {
                let mut workspace = Workspace {
                    id: id.clone(),
                    path: stored.path.clone()?,
                    repository: None,
                    source_control: crate::protocol::SourceControlAvailability::NotDetected,
                    icon: None,
                    description: None,
                };
                state.dress_workspace(&mut workspace);
                Some(workspace)
            })
            .collect::<Vec<_>>();
        workspaces.sort_by(|left, right| {
            recency(left)
                .cmp(&recency(right))
                .then_with(|| left.path.cmp(&right.path))
        });
        workspaces
    }

    /// Enters a Workspace in the Registry the moment it is seen: an entry
    /// known by `provenance` where it has none, presented at `presented`
    /// where that says anywhere. An entry it already has keeps how it came
    /// to be known and everything it carries, and is only moved to where the
    /// Workspace is presented now — a main root learned since, say. The
    /// durable write is queued under the store's own lock, so it lands in
    /// the order the store decided it, among the Icon and Description writes
    /// beside it. Answers the entry.
    pub(super) fn enter_workspace<'state>(
        &self,
        state: &'state mut SessionStoreState,
        workspace_id: &WorkspaceId,
        presented: Option<PathBuf>,
        provenance: WorkspaceProvenance,
    ) -> &'state mut StoredWorkspace {
        let stored = state
            .workspaces
            .entry(workspace_id.clone())
            .or_insert_with(|| StoredWorkspace::known_by(provenance));
        if let Some(path) = presented
            && stored.path.as_ref() != Some(&path)
        {
            stored.path = Some(path.clone());
            self.storage
                .record_workspace(workspace_id.clone(), path, stored.provenance.clone());
        }
        stored
    }

    pub(crate) async fn discover_workspaces(
        &self,
        source_control: &SourceControlService,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut sessions = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .map(|record| record.snapshot.session.clone())
            .collect::<Vec<_>>();
        for session in &sessions {
            source_control.remember(&session.workspace);
        }
        // Present directories first: a missing one then reuses the batch's
        // reading of its Repository instead of reading the metadata directory.
        sessions.sort_by_key(|session| !session.execution_directory.path.is_dir());
        let session_count = sessions.len();
        let mut resolutions = 0usize;
        let mut discovered: std::collections::HashMap<
            std::path::PathBuf,
            crate::protocol::ResolvedWorkspace,
        > = std::collections::HashMap::new();
        let mut batch = crate::source_control::DiscoveryBatch::default();
        for session in sessions {
            let resolution =
                if let Some(resolution) = discovered.get(&session.execution_directory.path) {
                    resolution.clone()
                } else if let Some(resolution) = discovered.values().find_map(|resolution| {
                    source_control.reuse_discovery(&session.execution_directory.path, resolution)
                }) {
                    discovered.insert(session.execution_directory.path.clone(), resolution.clone());
                    resolution
                } else {
                    resolutions += 1;
                    let resolution = source_control
                        .resolve_in_batch(
                            &mut batch,
                            &session.execution_directory.path,
                            Some(&session.workspace),
                        )
                        .await;
                    discovered.insert(session.execution_directory.path.clone(), resolution.clone());
                    resolution
                };
            // A retained working copy is an execution identity, not a path hint.
            // A different repository or checkout replacing that path cannot
            // inherit the Session or its opaque Provider Resume State.
            if let Some(known) = &session.checkout
                && (resolution.workspace.id != session.workspace.id
                    || resolution
                        .checkout
                        .as_ref()
                        .is_some_and(|actual| actual.id != known.id))
            {
                continue;
            }
            self.regroup(
                session.id,
                resolution.workspace,
                resolution.checkout.or(session.checkout),
            )?;
        }
        let result = self.refresh_repository_labels(source_control);
        tracing::info!(
            sessions = session_count,
            directories = discovered.len(),
            resolutions,
            elapsed_ms = started.elapsed().as_millis(),
            "Workspace discovery completed"
        );
        result
    }
    pub(crate) fn refresh_repository_labels(
        &self,
        source_control: &SourceControlService,
    ) -> anyhow::Result<()> {
        let workspaces = source_control.workspaces();
        let changes = self
            .state
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter_map(|record| {
                let session = &record.snapshot.session;
                workspaces
                    .iter()
                    .find(|workspace| workspace.id == session.workspace.id)
                    .map(|workspace| (session.id, workspace.clone(), session.checkout.clone()))
            })
            .collect::<Vec<_>>();
        for (id, workspace, checkout) in changes {
            self.regroup(id, workspace, checkout)?;
        }
        Ok(())
    }
    pub(crate) fn regroup(
        &self,
        id: SessionId,
        mut workspace: Workspace,
        mut checkout: Option<crate::protocol::CheckoutAssociation>,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.sessions.contains_key(&id) {
            return Ok(());
        }
        // A Session found working in a Workspace makes it known, whatever
        // else regrouping it changes.
        self.enter_workspace(
            &mut state,
            &workspace.id,
            Some(workspace.path.clone()),
            WorkspaceProvenance::Session,
        );
        // The table is authoritative for a Workspace's Icon and Description:
        // whatever resolution or a caller handed in, the current durable
        // reading wins, so every regrouped Session's own copy of its
        // Workspace stays in step with it (see
        // `SessionStore::commit_workspace_icon`, which reaches every Session
        // sharing a Workspace through this same path, as a Description's
        // landing does).
        state.dress_workspace(&mut workspace);
        let Some(record) = state.sessions.get_mut(&id) else {
            return Ok(());
        };
        if let Some(checkout) = &mut checkout
            && let Some(previous) = &record.snapshot.session.checkout
            && previous.id == checkout.id
        {
            if checkout.recovery_revision.is_none() {
                checkout.recovery_revision = previous.recovery_revision.clone();
            }
            if checkout.reclaim.is_none() {
                checkout.reclaim = previous.reclaim.clone();
            }
        }
        if record.snapshot.session.workspace == workspace
            && record.snapshot.session.checkout == checkout
        {
            return Ok(());
        }
        let mut next = record.snapshot.session.clone();
        next.workspace.clone_from(&workspace);
        next.checkout.clone_from(&checkout);
        let update = crate::protocol::SessionUpdate {
            session_id: id,
            revision: crate::protocol::SessionRevision(
                record
                    .snapshot
                    .revision
                    .0
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Session revision exhausted"))?,
            ),
            changes: vec![SessionChange::WorkspaceChanged {
                workspace,
                checkout,
            }],
        };
        // Storage takes the location before the Session does, so one it
        // refuses leaves the Session where it was.
        self.storage
            .location_changed(record.take_save()?, next, update.revision)?;
        crate::session_projection::apply_update(&mut record.snapshot, &update)?;
        record.summary.session = record.snapshot.session.clone();
        let _ = record.updates.send(update);
        if record.snapshot.session.parent.is_none() {
            state.publish_catalog_change(crate::protocol::SessionCatalogChange::Invalidated {
                session_id: id,
            });
        }
        // A Sidekick's tree names the Workspace of each Session beneath it.
        state.announce_subagent_tree(id);
        Ok(())
    }
}
