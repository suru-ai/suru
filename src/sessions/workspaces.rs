use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use super::{SessionStore, SessionStoreState};
use crate::{
    protocol::{
        Repository, RepositoryId, RepositoryLocation, SessionCatalogChange, SessionChange,
        SessionId, SourceControlAvailability, SourceControlCapabilities, SourceControlCapability,
        Workspace, WorkspaceId,
    },
    source_control::SourceControlService,
    storage::{StoredWorkspace, WorkspacePresentation, WorkspaceProvenance},
};

/// Why a Repository its Registry entry presents, but which this Server has
/// not read since it started, offers nothing yet.
const UNREAD_REPOSITORY: &str = "Suru has not read this Repository since it started";

/// The Repository an entry presented at `path` stands for, as far as how
/// that path presents it says: where it is presented by its bare root or by
/// its repository metadata location, the path is that metadata's, so the
/// Repository is known by its identity and its location without being read
/// — which is what names an entry whose main checkout is not known as its
/// Sessions name it. It is offered as unread, so nothing is asked of it until
/// source control reads it. A plain directory has none, and a main root does
/// not say where its metadata lies, so neither is presented with one.
fn presented_repository(
    workspace_id: &WorkspaceId,
    path: &Path,
    presentation: WorkspacePresentation,
) -> Option<Repository> {
    let location = match presentation {
        WorkspacePresentation::BareRoot => RepositoryLocation::Bare {
            root: path.to_owned(),
        },
        WorkspacePresentation::RepositoryMetadata => RepositoryLocation::UnknownMain,
        WorkspacePresentation::Directory | WorkspacePresentation::MainRoot => return None,
    };
    let (system, _) = workspace_id.0.split_once(':')?;
    let unread = || SourceControlCapability::Unsupported {
        reason: UNREAD_REPOSITORY.to_owned(),
    };
    Some(Repository {
        id: RepositoryId(workspace_id.0.clone()),
        system: system.to_owned(),
        metadata_directory: path.to_owned(),
        location,
        availability: SourceControlAvailability::Unavailable {
            reason: UNREAD_REPOSITORY.to_owned(),
        },
        capabilities: SourceControlCapabilities {
            list_checkouts: unread(),
            create_checkout: unread(),
            recover_checkout: unread(),
            remove_checkout: unread(),
            rename_branch: unread(),
        },
    })
}

/// When each Registry entry was last seen: a Session resolved there,
/// discovery finding it, or someone describing it.
#[derive(Default)]
pub(super) struct Sightings {
    /// How many sightings there have been.
    count: u64,
    /// The sighting each entry was last seen at.
    last: HashMap<WorkspaceId, u64>,
}

/// A moment as the Registry counts sightings: everything seen at it or
/// before, and nothing after.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sighting(u64);

impl Sightings {
    fn see(&mut self, workspace_id: &WorkspaceId) {
        self.count += 1;
        self.last.insert(workspace_id.clone(), self.count);
    }

    fn now(&self) -> Sighting {
        Sighting(self.count)
    }

    /// Whether `workspace_id` has been seen since `since`.
    fn seen_since(&self, workspace_id: &WorkspaceId, since: Sighting) -> bool {
        self.last
            .get(workspace_id)
            .is_some_and(|&last| last > since.0)
    }

    fn forget(&mut self, workspace_id: &WorkspaceId) {
        self.last.remove(workspace_id);
    }
}

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
    /// Announces every change of what [`Self::listed_workspaces`] lists: an
    /// entry made, presented anew, or dropped, its Icon or Description
    /// changed, or a Session made or deleted, which moves where its
    /// Workspace stands — so the listing a Server pushes moves with it.
    pub(crate) fn registry_moves(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.lock().unwrap().registry_moves.subscribe()
    }

    /// Every Workspace this server knows, as its Workspace Registry lists
    /// them (ADR 0051): first those the Sessions a listing holds work in —
    /// every top-level Session's that says, readable or not, as
    /// [`Self::list`] lists them — most recently worked in first; then the
    /// rest, by path. Each is given as its entry presents it — its identity,
    /// the path it was last presented by, and as much of its Repository as
    /// how that path presents it says (see [`presented_repository`]) — and
    /// dressed in what the entry carries; the Sessions only order them. An
    /// entry that says nowhere, having landed before rows kept a path, cannot
    /// be named and is left out until a Session working there presents it.
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
                let path = stored.path.clone()?;
                let repository = stored
                    .presentation
                    .and_then(|presentation| presented_repository(id, &path, presentation));
                let mut workspace = Workspace {
                    id: id.clone(),
                    path,
                    source_control: repository
                        .as_ref()
                        .map_or(SourceControlAvailability::NotDetected, |repository| {
                            repository.availability.clone()
                        }),
                    repository: repository.map(Box::new),
                    icon: None,
                    description: None,
                    unavailable: None,
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

    /// Enters `workspace`, which a Sidekick just Cloned to `directory`, in
    /// the Registry: known by its Clone where it has no entry yet, and
    /// otherwise only moved to where it is presented now (see
    /// [`Self::enter_workspace`]). Whatever another identity was entered at
    /// that directory is superseded, as wherever else a Workspace is first
    /// seen, so what it carried passes to `workspace`, and every Session
    /// grouped there wears it. Answers `workspace` dressed in what its entry
    /// carries.
    pub(crate) fn enter_cloned(&self, workspace: &Workspace, directory: &Path) -> Workspace {
        let mut dressed = workspace.clone();
        let inherited = {
            let mut state = self.state.lock().unwrap();
            self.enter_workspace(
                &mut state,
                &workspace.id,
                Some(workspace),
                WorkspaceProvenance::Cloned,
            );
            let inherited = self.retire_superseded(&mut state, workspace, Some(directory), None);
            state.dress_workspace(&mut dressed);
            inherited
        };
        if inherited {
            self.regroup_workspace(&workspace.id);
        }
        dressed
    }

    /// Enters a Workspace in the Registry the moment it is seen: an entry
    /// known by `provenance` where it has none, presented where and as
    /// `presented` — the Workspace as it was just resolved — presents it.
    /// An entry it already has keeps how it came to be known and everything
    /// it carries, and is only moved to where, and as what, the Workspace is
    /// presented now — a main root learned since, say. The durable write is
    /// queued under the store's own lock, so it lands in the order the store
    /// decided it, among the Icon and Description writes beside it. Answers
    /// the entry.
    pub(super) fn enter_workspace<'state>(
        &self,
        state: &'state mut SessionStoreState,
        workspace_id: &WorkspaceId,
        presented: Option<&Workspace>,
        provenance: WorkspaceProvenance,
    ) -> &'state mut StoredWorkspace {
        state.sightings.see(workspace_id);
        let mut moved = !state.workspaces.contains_key(workspace_id);
        let stored = state
            .workspaces
            .entry(workspace_id.clone())
            .or_insert_with(|| StoredWorkspace::known_by(provenance));
        if let Some(workspace) = presented {
            let presentation = WorkspacePresentation::of(workspace);
            if stored.path.as_ref() != Some(&workspace.path)
                || stored.presentation != Some(presentation)
            {
                stored.path = Some(workspace.path.clone());
                stored.presentation = Some(presentation);
                self.storage.record_workspace(
                    workspace_id.clone(),
                    workspace.path.clone(),
                    presentation,
                    stored.provenance.clone(),
                );
                moved = true;
            }
        }
        if moved {
            state.registry_moved();
        }
        state
            .workspaces
            .get_mut(workspace_id)
            .expect("the entry was just made where it was not")
    }

    /// Drops every entry a resolution of `workspace` has superseded: one
    /// presented, under another identity, where `workspace` is presented or
    /// at `directory`, the directory that was resolved — a plain directory's
    /// own Workspace whose directory has since become a Repository's, say —
    /// since that path belongs to `workspace` now. An entry a Session still
    /// belongs to stands until that Session's own directory is resolved
    /// again, `moving`, the Session this resolution regroups, aside. The
    /// Icon and the Description a dropped entry carried pass to `workspace`'s
    /// entry where it has none of its own, and every Client hears of them, so
    /// nothing the user described is lost. Call it once `workspace` is
    /// entered. Answers whether anything passed, which the Sessions already
    /// grouped under `workspace` must then be regrouped to wear.
    pub(super) fn retire_superseded(
        &self,
        state: &mut SessionStoreState,
        workspace: &Workspace,
        directory: Option<&Path>,
        moving: Option<SessionId>,
    ) -> bool {
        let superseded = state
            .workspaces
            .iter()
            .filter(|(id, stored)| {
                *id != &workspace.id
                    && stored
                        .path
                        .as_deref()
                        .is_some_and(|path| path == workspace.path || Some(path) == directory)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut inherited = false;
        for retired_id in superseded {
            if state.grouped_besides(&retired_id, moving) {
                continue;
            }
            let Some(retired) = state.workspaces.remove(&retired_id) else {
                continue;
            };
            state.sightings.forget(&retired_id);
            state.registry_moved();
            tracing::info!(
                superseded = %retired_id.0,
                workspace = %workspace.id.0,
                path = %workspace.path.display(),
                "a Workspace Registry entry was superseded at its path"
            );
            self.storage.forget_workspace(retired_id);
            let Some(entry) = state.workspaces.get_mut(&workspace.id) else {
                continue;
            };
            let provenance = entry.provenance.clone();
            let icon = retired.icon.filter(|_| entry.icon.is_none());
            let description = retired.description.filter(|_| entry.description.is_none());
            if let Some(icon) = &icon {
                entry.icon = Some(icon.clone());
                self.storage.save_workspace_icon(
                    workspace.id.clone(),
                    icon.clone(),
                    provenance.clone(),
                );
            }
            if let Some(description) = &description {
                entry.description = Some(description.clone());
                self.storage.save_workspace_description(
                    workspace.id.clone(),
                    description.clone(),
                    provenance,
                );
            }
            if let Some(icon) = icon {
                inherited = true;
                state.publish_catalog_change(SessionCatalogChange::WorkspaceIconChanged {
                    workspace_id: workspace.id.clone(),
                    icon: Some(icon),
                });
            }
            if let Some(description) = description {
                inherited = true;
                state.publish_catalog_change(SessionCatalogChange::WorkspaceDescriptionChanged {
                    workspace_id: workspace.id.clone(),
                    description: Some(description),
                });
            }
        }
        inherited
    }

    /// Enters `workspace`, which discovery found presented at `directory`
    /// directly within the Workspace Root `root`, in the Registry: known by
    /// discovery under that Root where it has no entry yet, and otherwise
    /// only moved to where it is presented now (see
    /// [`Self::enter_workspace`]). Whatever another identity was entered at
    /// that directory is superseded, as wherever else a Workspace is first
    /// seen, so what it carried passes to `workspace` and every Session
    /// grouped there wears it.
    pub(crate) fn enter_discovered(&self, workspace: &Workspace, directory: &Path, root: &str) {
        let inherited = {
            let mut state = self.state.lock().unwrap();
            self.enter_workspace(
                &mut state,
                &workspace.id,
                Some(workspace),
                WorkspaceProvenance::Discovered {
                    root: root.to_owned(),
                },
            );
            self.retire_superseded(&mut state, workspace, Some(directory), None)
        };
        if inherited {
            self.regroup_workspace(&workspace.id);
        }
    }

    /// The Workspace Root the entry `workspace_id` was discovered under, as
    /// the Setting spelled it then, where it was.
    pub(crate) fn discovered_under(&self, workspace_id: &WorkspaceId) -> Option<String> {
        match &self
            .state
            .lock()
            .unwrap()
            .workspaces
            .get(workspace_id)?
            .provenance
        {
            WorkspaceProvenance::Discovered { root } => Some(root.clone()),
            _ => None,
        }
    }

    /// Drops the entry `workspace_id`, which discovery under a Workspace
    /// Root found presented at a Workspace Root, where it is known by
    /// discovery under that Root — spelled as one of `spellings` — and no
    /// Session belongs to it, its Icon and Description with it: a Root is
    /// never a Workspace. Every entry known otherwise is left alone.
    pub(crate) fn forget_discovered(&self, workspace_id: &WorkspaceId, spellings: &[String]) {
        let mut state = self.state.lock().unwrap();
        let Some(root) = state
            .workspaces
            .get(workspace_id)
            .and_then(|stored| match &stored.provenance {
                WorkspaceProvenance::Discovered { root } => {
                    spellings.contains(root).then_some(root)
                }
                _ => None,
            })
            .cloned()
        else {
            return;
        };
        if state.grouped_besides(workspace_id, None) {
            return;
        }
        state.workspaces.remove(workspace_id);
        state.sightings.forget(workspace_id);
        state.registry_moved();
        tracing::info!(
            workspace = %workspace_id.0,
            root,
            "a Workspace Registry entry discovered under a Workspace Root was dropped, since it is \
             presented at a Workspace Root"
        );
        self.storage.forget_workspace(workspace_id.clone());
    }

    /// Every entry the Registry holds a path for, by its identity and the
    /// path it is presented at — where each is read for its standing — and
    /// the moment they were taken, which a reading of them began after.
    pub(crate) fn presented_entries(&self) -> (Sighting, Vec<(WorkspaceId, PathBuf)>) {
        let state = self.state.lock().unwrap();
        let entries = state
            .workspaces
            .iter()
            .filter_map(|(id, stored)| Some((id.clone(), stored.path.clone()?)))
            .collect();
        (state.sightings.now(), entries)
    }

    /// Drops the entry `workspace_id` names, found gone from `path` by a
    /// reading begun after `since` while the directory holding it could be
    /// read, its Icon and Description with it — unless a Session belongs to
    /// it, which keeps it. One seen since that reading began, or presented
    /// elsewhere since, is left alone, since the reading no longer says what
    /// is there. Answers whether the entry stands gone at `path` still.
    pub(crate) fn forget_gone(
        &self,
        workspace_id: &WorkspaceId,
        path: &Path,
        since: Sighting,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let presented_there = state
            .workspaces
            .get(workspace_id)
            .is_some_and(|stored| stored.path.as_deref() == Some(path));
        if !presented_there || state.sightings.seen_since(workspace_id, since) {
            return false;
        }
        if state.grouped_besides(workspace_id, None) {
            return true;
        }
        state.workspaces.remove(workspace_id);
        state.sightings.forget(workspace_id);
        state.registry_moved();
        tracing::info!(
            workspace = %workspace_id.0,
            path = %path.display(),
            "a Workspace Registry entry was dropped, its directory gone"
        );
        self.storage.forget_workspace(workspace_id.clone());
        false
    }

    /// Every entry discovered under a Workspace Root: its identity, that
    /// Root as the Setting spelled it then, and the path it is presented at,
    /// where it has one.
    pub(crate) fn discovered_entries(&self) -> Vec<DiscoveredEntry> {
        self.state
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .filter_map(|(id, stored)| match &stored.provenance {
                WorkspaceProvenance::Discovered { root } => Some(DiscoveredEntry {
                    id: id.clone(),
                    root: root.clone(),
                    path: stored.path.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Settles each entry `rerooted` names, where it is known still by
    /// discovery under the Workspace Root `rerooted` names beside it — so an
    /// entry entered or known otherwise since `rerooted` was decided is left
    /// alone. One no Root in force holds is dropped, its Icon and
    /// Description with it — unless a Session belongs to it, which keeps it
    /// known by that Session from now on, so it stands as any entry a
    /// Session made does, whatever later becomes of the Session or the
    /// Roots. One another spelling of the Roots in force holds is known by
    /// discovery under that spelling from now on, so taking that spelling
    /// away later takes it.
    pub(crate) fn reroot_discovered(&self, rerooted: HashMap<WorkspaceId, (String, Rerooting)>) {
        let mut state = self.state.lock().unwrap();
        for (workspace_id, (root, rerooting)) in rerooted {
            let still_under_root = state.workspaces.get(&workspace_id).is_some_and(|stored| {
                matches!(
                    &stored.provenance,
                    WorkspaceProvenance::Discovered { root: under } if *under == root
                )
            });
            if !still_under_root {
                continue;
            }
            let provenance = match rerooting {
                Rerooting::Respelled(spelling) => {
                    WorkspaceProvenance::Discovered { root: spelling }
                }
                Rerooting::Unrooted if state.grouped_besides(&workspace_id, None) => {
                    WorkspaceProvenance::Session
                }
                Rerooting::Unrooted => {
                    state.workspaces.remove(&workspace_id);
                    state.sightings.forget(&workspace_id);
                    state.registry_moved();
                    tracing::info!(
                        workspace = %workspace_id.0,
                        root,
                        "a Workspace Registry entry was dropped with the Workspace Root it was \
                         discovered under"
                    );
                    self.storage.forget_workspace(workspace_id);
                    continue;
                }
            };
            if let Some(stored) = state.workspaces.get_mut(&workspace_id) {
                stored.provenance = provenance.clone();
                self.storage
                    .record_workspace_provenance(workspace_id, provenance);
            }
        }
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
    /// Regroups a Session under `workspace`, which resolving its directory
    /// found it works in — and, with `checkout`, the Worktree it works in.
    pub(crate) fn regroup(
        &self,
        id: SessionId,
        workspace: Workspace,
        checkout: Option<crate::protocol::CheckoutAssociation>,
    ) -> anyhow::Result<()> {
        let workspace_id = workspace.id.clone();
        if self.regroup_as(id, workspace, checkout, Regrouping::Resolved)? {
            // What a superseded entry carried now dresses the Workspace, so
            // every Session already grouped there wears it too.
            self.regroup_workspace(&workspace_id);
        }
        Ok(())
    }

    /// [`Self::regroup`], for a Workspace `regrouping` says was resolved for
    /// the Session's directory, or is only its own copy dressed again.
    /// Answers whether the Workspace's entry took on what a superseded entry
    /// carried.
    pub(super) fn regroup_as(
        &self,
        id: SessionId,
        mut workspace: Workspace,
        mut checkout: Option<crate::protocol::CheckoutAssociation>,
        regrouping: Regrouping,
    ) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        let Some(directory) = state
            .sessions
            .get(&id)
            .map(|record| record.snapshot.session.execution_directory.path.clone())
        else {
            return Ok(false);
        };
        // A Session found working in a Workspace makes it known, whatever
        // else regrouping it changes; and where resolution found it there,
        // whatever another identity was entered for its directory, or for
        // where the Workspace is presented, is superseded — though this
        // Session no longer keeps it.
        self.enter_workspace(
            &mut state,
            &workspace.id,
            Some(&workspace),
            WorkspaceProvenance::Session,
        );
        let inherited = regrouping == Regrouping::Resolved
            && self.retire_superseded(&mut state, &workspace, Some(&directory), Some(id));
        // The table is authoritative for a Workspace's Icon and Description:
        // whatever resolution or a caller handed in, the current durable
        // reading wins, so every regrouped Session's own copy of its
        // Workspace stays in step with it (see
        // `SessionStore::commit_workspace_icon`, which reaches every Session
        // sharing a Workspace through this same path, as a Description's
        // landing does).
        state.dress_workspace(&mut workspace);
        let Some(record) = state.sessions.get_mut(&id) else {
            return Ok(inherited);
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
            return Ok(inherited);
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
        Ok(inherited)
    }

    /// Every Repository the Workspace Registry holds an entry for, by its
    /// identity and the path its entry presents it at — its main root, its
    /// bare root, or its repository metadata location — whether or not a
    /// Session works there any longer, in the order of those paths: what the
    /// Reclaim sweep finds a Repository by after a restart once nothing else
    /// names it (ADR 0051).
    pub(crate) fn registered_repositories(&self) -> Vec<(RepositoryId, PathBuf)> {
        let mut repositories = self
            .state
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .filter(|(_, stored)| {
                matches!(
                    stored.presentation,
                    Some(
                        WorkspacePresentation::MainRoot
                            | WorkspacePresentation::BareRoot
                            | WorkspacePresentation::RepositoryMetadata
                    )
                )
            })
            .filter_map(|(id, stored)| Some((RepositoryId(id.0.clone()), stored.path.clone()?)))
            .collect::<Vec<_>>();
        repositories.sort_by(|(_, left), (_, right)| left.cmp(right));
        repositories
    }
}

/// A Registry entry discovered under a Workspace Root, as
/// [`SessionStore::discovered_entries`] answers it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredEntry {
    pub(crate) id: WorkspaceId,
    /// The Root it was discovered under, as the Setting spelled it then.
    pub(crate) root: String,
    /// Where it is presented: directly within the directory that Root
    /// named when it was discovered there, but for a linked Worktree's
    /// Repository, presented by its main or bare root wherever that is.
    pub(crate) path: Option<PathBuf>,
}

/// What becomes of an entry discovered under a Workspace Root once the
/// Roots in force no longer spell that Root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Rerooting {
    /// No Root in force holds it, so it goes with the Root it was
    /// discovered under.
    Unrooted,
    /// The Root in force spelled so holds it: the same directory spelled
    /// another way, or one that may be, named in the very change that took
    /// the other spelling away.
    Respelled(String),
}

/// Whether the Workspace a Session is regrouped under was just resolved for
/// its directory — which is what may find another entry at that path
/// superseded — or is only the Session's own copy, dressed again in what the
/// Registry holds for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Regrouping {
    Resolved,
    Redressed,
}
