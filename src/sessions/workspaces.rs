use super::{SessionStore, SessionStoreState};
use crate::{
    protocol::{SessionChange, SessionId, Workspace, WorkspaceId},
    source_control::SourceControlService,
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
    /// Every Workspace this server knows, as a listing of Workspaces names
    /// them: first those the Sessions a listing holds work in — every
    /// top-level Session's that says, readable or not, as [`Self::list`]
    /// lists them — each once, most recently worked in first; then those it
    /// holds a Description or Icon for though no such Session works in them,
    /// by path. Each is dressed in what the `workspaces` table holds for it
    /// now. One known only by its row is given as its row presents it — its
    /// identity, the path it was last presented by, and what it carries —
    /// with nothing of its Repository, which only a resolution reads, so no
    /// source control is detected for it; one whose row says nowhere, having
    /// landed before rows kept a path, cannot be named and is left out.
    pub(crate) fn listed_workspaces(&self) -> Vec<Workspace> {
        let listed = self.list(None);
        let state = self.state.lock().unwrap();
        let mut workspaces = crate::protocol::distinct_workspaces(
            listed
                .iter()
                .filter_map(crate::protocol::SessionListItem::workspace),
        );
        // A Session Suru could not read keeps the copy of its Workspace it
        // was restored with, which the table may since have moved past.
        for workspace in &mut workspaces {
            state.dress_workspace(workspace);
        }
        let mut unworked = state
            .workspaces
            .iter()
            .filter(|(id, _)| !workspaces.iter().any(|workspace| &workspace.id == *id))
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
        unworked.sort_by(|left, right| left.path.cmp(&right.path));
        workspaces.extend(unworked);
        workspaces
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
