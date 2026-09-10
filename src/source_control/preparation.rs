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
}
