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

/// What every scripted program waits through rather than looping on its own, so that none runs on
/// once its test is over — whatever failed to stop it.
///
/// A program holding at a gate the test never opens, or idling as a stand-in for a harness that has
/// stopped answering, would otherwise run forever once nothing kills it: the test's directory is
/// gone, so the gate can never appear, and a part of the program run in the background reads its
/// stdin from `/dev/null`, so no closing pipe ends it either. So:
///
/// - `test_running` holds while the directory holding the program stands — it goes once the test
///   lets go of its fixture — and the test process that wrote the program lives, which it does not
///   once killed at a timeout, say, with nothing deleted;
/// - `fixture_running` holds while the test runs and the program's main process lives too, which
///   is how a part of it run in the background learns that its main process has exited. `$$`
///   names the main process in a subshell too, but is captured before anything runs, to say so;
/// - `wait_for FILE` waits until FILE appears, and `idle_forever` for nothing at all. Either ends
///   the process waiting as soon as the fixture is no longer running, with status 1, since it never
///   got what it waited for.
///
/// A fixture a test kills to see whether Suru takes what it started down with it waits on
/// `test_running` alone, since a part that ended with its main process would hide Suru failing to.
/// The checks are builtins, so each poll costs what the bare `sleep` loop it replaced did.
const FIXTURE_LIFETIME: &str = r#"fixture_main=$$
test_running() {
  [ -d "$fixture_dir" ] && kill -0 "$fixture_test" 2>/dev/null
}
fixture_running() {
  test_running && kill -0 "$fixture_main" 2>/dev/null
}
wait_for() {
  while [ ! -e "$1" ]; do
    fixture_running || exit 1
    sleep 0.01
  done
}
idle_forever() {
  while fixture_running; do
    sleep 0.1
  done
  exit 1
}
"#;

/// Writes `script` at `path` as a program the runtime can launch, with [`FIXTURE_LIFETIME`]'s waits
/// and [`APPEND_LINE`] defined. Those waits watch the directory holding the program, so it should
/// be the test's own, and the process writing it, which is the test's.
pub fn write_executable(path: &Path, script: &str) {
    let body = script
        .strip_prefix(SHEBANG)
        .unwrap_or_else(|| panic!("scripted executable {path:?} opens with {SHEBANG:?}"));
    let directory = path
        .parent()
        .and_then(Path::to_str)
        .unwrap_or_else(|| panic!("scripted executable {path:?} sits in a UTF-8 directory"))
        .replace('\'', r"'\''");
    let test = std::process::id();
    std::fs::write(
        path,
        format!(
            "{SHEBANG}fixture_dir='{directory}'\nfixture_test={test}\n{FIXTURE_LIFETIME}{APPEND_LINE}{body}"
        ),
    )
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
