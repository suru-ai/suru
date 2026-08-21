use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

use anyhow::{Context, Result};
use blake3::Hasher;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

/// Hashing a large executable is expensive and the result only changes when
/// the file does, so identities are memoized per path and invalidated by the
/// file's modification time and length.
static IDENTITY_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedIdentity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct CachedIdentity {
    modified: SystemTime,
    len: u64,
    identity: String,
}

pub fn for_executable(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect Suru executable for build identity: {path:?}"))?;
    let modified = metadata.modified().ok();
    if let Some(modified) = modified
        && let Some(cached) = IDENTITY_CACHE
            .lock()
            .expect("build identity cache lock is not poisoned")
            .get(path)
        && cached.modified == modified
        && cached.len == metadata.len()
    {
        return Ok(cached.identity.clone());
    }

    let mut executable = File::open(path)
        .with_context(|| format!("open Suru executable for build identity: {path:?}"))?;
    let mut hasher = Hasher::new();
    let mut buffer = [0; HASH_BUFFER_SIZE];
    loop {
        let bytes_read = executable
            .read(&mut buffer)
            .with_context(|| format!("read Suru executable for build identity: {path:?}"))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let identity = format!(
        "{}@{}+blake3:{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        hasher.finalize().to_hex()
    );

    if let Some(modified) = modified {
        IDENTITY_CACHE
            .lock()
            .expect("build identity cache lock is not poisoned")
            .insert(
                path.to_owned(),
                CachedIdentity {
                    modified,
                    len: metadata.len(),
                    identity: identity.clone(),
                },
            );
    }
    Ok(identity)
}

pub fn for_current_executable() -> Result<String> {
    let executable = std::env::current_exe().context("find current Suru executable")?;
    for_executable(executable)
}
