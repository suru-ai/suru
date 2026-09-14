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
    pub(crate) serial: Arc<tokio::sync::Mutex<()>>,
}
impl PreparationStore {
    pub(crate) fn new(data: &Path) -> Self {
        Self {
            root: data.join("checkout-preparations"),
            serial: Default::default(),
        }
    }
    pub(crate) fn load(&self, id: PreparationId) -> Result<Option<PreparedCheckout>, String> {
        let path = self.root.join(format!("{}.json", id.0));
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
            file.persist(self.root.join(format!("{}.json", preparation.id.0)))?;
            Ok(())
        })()
        .map_err(|e| format!("Cannot persist Worktree preparation: {e}"))
    }

    pub(crate) fn delete(&self, id: PreparationId) -> Result<(), String> {
        match std::fs::remove_file(self.root.join(format!("{}.json", id.0))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Cannot delete Worktree preparation: {e}")),
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
        let deleted = self.delete(preparation.id);
        match (saved, deleted) {
            (_, Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(save), Err(delete)) => Err(format!("{save}; {delete}")),
        }
    }

    pub(crate) fn delete_for_destination(&self, destination: &Path) -> Result<(), String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Cannot read Worktree preparations: {e}")),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(preparation) = serde_json::from_slice::<PreparedCheckout>(&bytes) else {
                continue;
            };
            if preparation.destination.path == destination {
                std::fs::remove_file(path)
                    .map_err(|e| format!("Cannot delete Worktree preparation: {e}"))?;
            }
        }
        Ok(())
    }

    /// The Sessions a stored preparation can still bring to their first Turn:
    /// an intention that never recorded an admitted Session is one whose
    /// creation did not finish, and rejoining it is how its Prompt reaches the
    /// Provider it never reached. That makes this the durable record of who
    /// still owes such a Prompt a Turn across a restart, which is what keeps
    /// restoration from withdrawing it (ADR 0024). An unreadable intention
    /// names no Session and holds nothing back.
    pub(crate) fn resumable_sessions(&self) -> Vec<crate::protocol::SessionId> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let bytes = std::fs::read(entry.ok()?.path()).ok()?;
                let preparation: PreparedCheckout = serde_json::from_slice(&bytes).ok()?;
                preparation
                    .admitted_session
                    .is_none()
                    .then_some(preparation.intended_session)
            })
            .collect()
    }
}
