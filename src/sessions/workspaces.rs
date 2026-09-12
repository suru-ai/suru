use super::SessionStore;
use crate::{
    protocol::{SessionChange, SessionId, Workspace, WorkspaceId},
    source_control::SourceControlService,
};

impl SessionStore {
    pub(crate) fn known_workspace(&self, id: &WorkspaceId) -> Option<Workspace> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .values()
            .find(|record| &record.summary.session.workspace.id == id)
            .map(|record| record.summary.session.workspace.clone())
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
            if let Some(known) = &session.checkout {
                if resolution.workspace.id != session.workspace.id
                    || resolution
                        .checkout
                        .as_ref()
                        .is_some_and(|actual| actual.id != known.id)
                {
                    continue;
                }
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
        workspace: Workspace,
        mut checkout: Option<crate::protocol::CheckoutAssociation>,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let Some(record) = state.sessions.get_mut(&id) else {
            return Ok(());
        };
        if let Some(checkout) = &mut checkout
            && checkout.recovery_revision.is_none()
            && let Some(previous) = &record.snapshot.session.checkout
            && previous.id == checkout.id
        {
            checkout.recovery_revision = previous.recovery_revision.clone();
        }
        if record.snapshot.session.workspace == workspace
            && record.snapshot.session.checkout == checkout
        {
            return Ok(());
        }
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
        crate::session_projection::apply_update(&mut record.snapshot, &update)?;
        record.summary.session = record.snapshot.session.clone();
        self.storage
            .location_changed(record.snapshot.session.clone(), record.snapshot.revision)?;
        let _ = record.updates.send(update);
        if record.snapshot.session.parent.is_none() {
            state.publish_catalog_change(crate::protocol::SessionCatalogChange::Invalidated {
                session_id: id,
            });
        }
        Ok(())
    }
}
