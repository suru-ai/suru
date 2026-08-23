//! Shared plumbing for the scripted stand-in binaries the Provider suites drive their runtimes
//! against. What a Provider's harness *says* is its own business — Codex speaks line-delimited
//! JSON-RPC and Copilot Content-Length-framed JSON-RPC — but writing the program and reading back
//! what it was asked is the same job either way.

use std::{os::unix::fs::PermissionsExt, path::Path};

use serde_json::Value;

/// Writes `script` at `path` as a program the runtime can launch.
pub fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script)
        .unwrap_or_else(|error| panic!("write scripted executable {path:?}: {error}"));
    let mut permissions = std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("read scripted executable metadata {path:?}: {error}"))
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("make scripted executable {path:?}: {error}"));
}

/// The requests a scripted program recorded, one JSON object per line. An absent log is no
/// requests, so a test may ask before the program has been launched.
pub fn captured_requests(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("decode captured request"))
        .collect()
}

/// Just the method names from [`captured_requests`], in the order they arrived.
pub fn captured_methods(log: &Path) -> Vec<String> {
    captured_requests(log)
        .into_iter()
        .filter_map(|request| {
            request
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}
