//! Recognizes the launcher wrapper Codex reports around the commands it runs.
//!
//! Codex launches every script through a shell — `/usr/bin/zsh -lc '<script>'`
//! on POSIX systems, `pwsh -Command '<script>'` on Windows — and reports the
//! whole invocation, shlex-joined, as the command. That wrapper is Codex
//! plumbing, not part of the command a reader should see, so the projection
//! strips it before the Activity is recorded. A command in any shape this
//! module does not recognize as Codex's own plumbing is kept verbatim.

use std::ffi::OsStr;
use std::path::Path;

/// Returns the command as a reader should see it: the script inside a
/// recognized launcher wrapper, or the command verbatim otherwise.
pub(super) fn strip_launcher_wrapper(command: String) -> String {
    let Some(argv) = split_round_trip(&command) else {
        return command;
    };
    match extract_posix_script(&argv).or_else(|| extract_powershell_script(&argv)) {
        Some(script) => script.to_owned(),
        None => command,
    }
}

/// Splits the shlex-joined command back into the argv Codex joined, refusing
/// when splitting is lossy: quoting shlex cannot parse, or a split that would
/// not faithfully re-join (as with Windows path escapes).
fn split_round_trip(command: &str) -> Option<Vec<String>> {
    let argv = shlex::split(command)?;
    let round_trip = shlex::try_join(argv.iter().map(String::as_str)).ok()?;
    // `:\` marks Windows drive or UNC paths, whose backslashes a POSIX split
    // reads as escapes; a re-join can then look faithful while the argv is
    // mangled, so only an exact round trip is trusted for them.
    if round_trip == command
        || (!command.contains(":\\") && shlex::split(&round_trip).as_ref() == Some(&argv))
    {
        Some(argv)
    } else {
        None
    }
}

/// The script inside a POSIX shell wrapper: exactly `<shell> -lc|-c <script>`
/// where the shell's basename is `bash`, `zsh`, or `sh`.
fn extract_posix_script(argv: &[String]) -> Option<&str> {
    let [shell, flag, script] = argv else {
        return None;
    };
    (matches!(flag.as_str(), "-lc" | "-c") && is_posix_shell(shell)).then_some(script)
}

fn is_posix_shell(shell: &str) -> bool {
    shell_name(shell).is_some_and(|name| matches!(name, "bash" | "zsh" | "sh"))
}

/// The script inside a PowerShell wrapper: `pwsh` or `powershell` followed by
/// a run of launcher flags with the script right after `-Command`/`-c`. Any
/// flag outside that run means the invocation is not Codex's plumbing.
fn extract_powershell_script(argv: &[String]) -> Option<&str> {
    let [shell, flags @ ..] = argv else {
        return None;
    };
    if !shell_name(shell).is_some_and(|name| matches!(name, "pwsh" | "powershell")) {
        return None;
    }
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if flag.eq_ignore_ascii_case("-Command") || flag.eq_ignore_ascii_case("-c") {
            return flags.next().map(String::as_str);
        }
        if !flag.eq_ignore_ascii_case("-NoLogo") && !flag.eq_ignore_ascii_case("-NoProfile") {
            return None;
        }
    }
    None
}

/// The name a shell is recognized by: the last path component with every
/// extension stripped, as Codex's own shell detection resolves it.
fn shell_name(shell: &str) -> Option<&str> {
    let mut name = Path::new(shell).file_name().and_then(OsStr::to_str)?;
    while let Some(stem) = Path::new(name).file_stem().and_then(OsStr::to_str) {
        if stem == name {
            break;
        }
        name = stem;
    }
    Some(name)
}
