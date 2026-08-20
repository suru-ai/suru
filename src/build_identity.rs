use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result};
use blake3::Hasher;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

pub fn for_executable(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
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

    Ok(format!(
        "{}@{}+blake3:{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        hasher.finalize().to_hex()
    ))
}

pub fn for_current_executable() -> Result<String> {
    let executable = std::env::current_exe().context("find current Suru executable")?;
    for_executable(executable)
}
