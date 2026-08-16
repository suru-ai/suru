use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const RUNTIME_FILE: &str = "runtime.json";
const LOCK_FILE: &str = "server.lock";

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    state_dir: PathBuf,
    channel: String,
}

impl RuntimeConfig {
    pub fn new(state_dir: impl AsRef<Path>, channel: impl Into<String>) -> Result<Self> {
        let channel = channel.into();
        let channel_is_safe = !channel.is_empty()
            && channel != "."
            && channel != ".."
            && channel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !channel_is_safe {
            bail!("channel must contain only letters, numbers, '.', '-', or '_'");
        }
        Ok(Self {
            state_dir: state_dir.as_ref().to_path_buf(),
            channel,
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.state_dir.join(&self.channel)
    }

    pub fn descriptor_path(&self) -> PathBuf {
        self.runtime_dir().join(RUNTIME_FILE)
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.runtime_dir().join(LOCK_FILE)
    }

    pub(crate) fn create_private_runtime_dir(&self) -> Result<PathBuf> {
        let runtime_dir = self.runtime_dir();
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("create runtime directory {runtime_dir:?}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("protect runtime directory {runtime_dir:?}"))?;
        }
        Ok(runtime_dir)
    }
}
