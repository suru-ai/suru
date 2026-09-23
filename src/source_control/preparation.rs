//! Durable first-Prompt intentions, independent of Session and Prompt lifetimes.
use crate::protocol::{PreparationId, PreparedCheckout};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(crate) struct PreparationStore {
    root: PathBuf,
    retired: PathBuf,
    pub(crate) serial: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
enum UnreadableIntent {
    Skip,
    Reject,
}

#[derive(Clone, Copy)]
enum RetiredIntent {
    Include,
    Exclude,
}

impl PreparationStore {
    pub(crate) fn new(data: &Path) -> Self {
        let store = Self {
            root: data.join("checkout-preparations"),
            retired: data.join("retired-checkout-preparations"),
            serial: Default::default(),
        };
        store.reconcile_retired();
        store
    }
    fn intent_path(&self, id: PreparationId) -> PathBuf {
        self.root.join(format!("{}.json", id.0))
    }
    fn retirement_path(&self, id: PreparationId) -> PathBuf {
        self.retired.join(format!("{}.retired", id.0))
    }
    fn is_retired(&self, id: PreparationId) -> bool {
        self.retirement_path(id).is_file()
    }
    fn reconcile_retired(&self) {
        let Ok(entries) = std::fs::read_dir(&self.retired) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
                .map(PreparationId)
            else {
                continue;
            };
            if let Err(error) = self.finish_retirement(id) {
                tracing::warn!(
                    preparation = %id.0,
                    "Retired Worktree preparation cleanup will retry after restart: {error}"
                );
            }
        }
    }
    pub(crate) fn load(&self, id: PreparationId) -> Result<Option<PreparedCheckout>, String> {
        if self.is_retired(id) {
            return Ok(None);
        }
        let path = self.intent_path(id);
        match std::fs::read(path) {
            Ok(bytes) => {
                let preparation: PreparedCheckout = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("Cannot read Worktree preparation: {e}"))?;
                if preparation.id != id {
                    return Err(
                        "Stored preparation identity conflicts with the requested intention".into(),
                    );
                }
                Ok(Some(preparation))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Cannot read Worktree preparation: {e}")),
        }
    }
    pub(crate) fn save(&self, preparation: &PreparedCheckout) -> Result<(), String> {
        (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(&self.root)?;
            let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
            file.write_all(&serde_json::to_vec(preparation)?)?;
            file.as_file().sync_all()?;
            file.persist(self.intent_path(preparation.id))?;
            Ok(())
        })()
        .map_err(|e| format!("Cannot persist Worktree preparation: {e}"))
    }

    pub(crate) fn retire(&self, id: PreparationId) -> Result<(), String> {
        let path = self.retirement_path(id);
        if path.is_file() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.retired)
            .map_err(|e| format!("Cannot retire Worktree preparation: {e}"))?;
        (|| -> anyhow::Result<()> {
            let mut marker = tempfile::NamedTempFile::new_in(&self.retired)?;
            marker.write_all(id.0.to_string().as_bytes())?;
            marker.as_file().sync_all()?;
            marker.persist(path)?;
            Ok(())
        })()
        .map_err(|e| format!("Cannot retire Worktree preparation: {e}"))
    }

    pub(crate) fn finish_retirement(&self, id: PreparationId) -> Result<(), String> {
        match std::fs::remove_file(self.intent_path(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Cannot delete Worktree preparation: {e}")),
        }
        match std::fs::remove_file(self.retirement_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "Cannot delete retired Worktree preparation marker: {e}"
            )),
        }
    }

    fn retire_and_delete(&self, id: PreparationId) -> Result<(), String> {
        let retired = self.retire(id);
        let deleted = self.finish_retirement(id);
        match (retired, deleted) {
            (_, Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(retire), Err(delete)) => Err(format!("{retire}; {delete}")),
        }
    }

    /// Records that admission completed before deleting the intent. If the
    /// marker write fails, still attempt deletion so an old unadmitted record
    /// cannot keep the Prompt resumable across restart.
    pub(crate) fn delete_after_admission(
        &self,
        preparation: &PreparedCheckout,
    ) -> Result<(), String> {
        let saved = self.save(preparation);
        let deleted = self.retire_and_delete(preparation.id);
        match (saved, deleted) {
            (_, Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(save), Err(delete)) => Err(format!("{save}; {delete}")),
        }
    }

    pub(crate) fn for_destination(
        &self,
        destination: &Path,
    ) -> Result<Vec<PreparedCheckout>, String> {
        Ok(self
            .intentions(UnreadableIntent::Skip, RetiredIntent::Include)?
            .into_iter()
            .filter(|preparation| preparation.destination.path == destination)
            .collect())
    }

    /// Where every live stored intention means to put its Worktree. A new
    /// plan never takes one of these, even before its branch or location
    /// exists, so no intention can be retried into another's name.
    pub(crate) fn intended_destinations(&self) -> Vec<PathBuf> {
        self.intentions(UnreadableIntent::Skip, RetiredIntent::Exclude)
            .unwrap_or_default()
            .into_iter()
            .map(|preparation| preparation.destination.path)
            .collect()
    }

    fn intentions(
        &self,
        unreadable: UnreadableIntent,
        retired: RetiredIntent,
    ) -> Result<Vec<PreparedCheckout>, String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("Cannot read Worktree preparations: {e}")),
        };
        let mut preparations = Vec::new();
        for entry in entries {
            let preparation = (|| {
                let entry = entry.map_err(|e| format!("Cannot read Worktree preparations: {e}"))?;
                let bytes = std::fs::read(entry.path())
                    .map_err(|e| format!("Cannot read Worktree preparation: {e}"))?;
                serde_json::from_slice::<PreparedCheckout>(&bytes)
                    .map_err(|e| format!("Cannot read Worktree preparation: {e}"))
            })();
            let preparation = match preparation {
                Ok(preparation) => preparation,
                Err(_) if matches!(unreadable, UnreadableIntent::Skip) => continue,
                Err(error) => return Err(error),
            };
            if matches!(retired, RetiredIntent::Include) || !self.is_retired(preparation.id) {
                preparations.push(preparation);
            }
        }
        Ok(preparations)
    }

    /// Every readable, live preparation intent. Persisted Repository facts in
    /// these records let startup Reclaim find failed preparations even when no
    /// admitted Session remains to seed Workspace discovery.
    pub(crate) fn all(&self) -> Result<Vec<PreparedCheckout>, String> {
        self.intentions(UnreadableIntent::Reject, RetiredIntent::Exclude)
    }

    /// The Sessions a stored preparation can still bring to their first Turn:
    /// an intention that never recorded an admitted Session is one whose
    /// creation did not finish, and rejoining it is how its Prompt reaches the
    /// Provider it never reached. That makes this the durable record of who
    /// still owes such a Prompt a Turn across a restart, which is what keeps
    /// restoration from withdrawing it (ADR 0024). An unreadable intention
    /// names no Session and holds nothing back.
    pub(crate) fn resumable_sessions(&self) -> Vec<crate::protocol::SessionId> {
        self.intentions(UnreadableIntent::Skip, RetiredIntent::Exclude)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|preparation| {
                preparation
                    .admitted_session
                    .is_none()
                    .then_some(preparation.intended_session)
            })
            .collect()
    }
}
