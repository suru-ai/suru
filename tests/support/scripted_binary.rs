//! Shared plumbing for the scripted stand-in binaries the Provider suites drive their runtimes
//! against. What a Provider's harness *says* is its own business — Codex speaks line-delimited
//! JSON-RPC and Copilot Content-Length-framed JSON-RPC — but writing the program and reading back
//! what it was asked is the same job either way.

use std::{os::unix::fs::PermissionsExt, path::Path};

use serde_json::Value;

const SHEBANG: &str = "#!/bin/sh\n";

/// `append_line FILE LINE`, which every scripted program can call to record a line.
///
/// macOS's shell writes a `printf` longer than 1KiB in 1KiB pieces, and another program appending
/// to the same file can land between them, splicing its line into the middle of this one. A long
/// line is therefore written by a single `cat` of a file holding just that line. Short lines keep
/// the builtin: a process per line would slow every program down, and some tests read the log
/// the moment the Server has sent the request it records.
const APPEND_LINE: &str = r#"append_line() {
  if [ ${#2} -gt 1000 ]; then
    printf '%s\n' "$2" > "$1.$$" && cat "$1.$$" >> "$1"
  else
    printf '%s\n' "$2" >> "$1"
  fi
}
"#;

/// Writes `script` at `path` as a program the runtime can launch, with [`APPEND_LINE`] defined.
pub fn write_executable(path: &Path, script: &str) {
    let body = script
        .strip_prefix(SHEBANG)
        .unwrap_or_else(|| panic!("scripted executable {path:?} opens with {SHEBANG:?}"));
    std::fs::write(path, format!("{SHEBANG}{APPEND_LINE}{body}"))
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
///
/// The programs append with [`APPEND_LINE`]'s `append_line`, so programs appending at once
/// never interleave mid-request.
pub fn captured_requests(log: &Path) -> Vec<Value> {
    let contents = std::fs::read_to_string(log).unwrap_or_default();
    let complete = contents.strip_suffix('\n').map_or_else(
        || contents.rsplit_once('\n').map_or("", |(lines, _)| lines),
        |lines| lines,
    );
    complete
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
