//! Operator-facing Log files: one per process run, under the state dir.
//!
//! The TUI client and the detached server initialize the same pipeline: a
//! `tracing` subscriber writing structured lines to `<state>/log/`, one file
//! per process run, pruned to the newest few so the directory stays bounded.
//! The launcher's `server.log` stdio redirect is deliberately separate: it is
//! the crash net that catches output no in-process subscriber can (panics,
//! pre-init failures). See ADR-0008.
//!
//! `SURU_LOG` decides how verbose the Log is, within one bound it cannot
//! lift: a dependency that logs the payloads it carries at its verbose levels
//! is held to its warnings and errors. rmcp, which serves the Broker, logs
//! every call it receives whole — a Sidekick's Answers among them, secret ones
//! included — before Suru has read it, so no directive may let those lines
//! through.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer,
    filter::{FilterExt, LevelFilter, Targets},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::runtime::{RuntimeConfig, protect_current_user_directory, protect_current_user_file};

const LOG_DIR: &str = "log";
const FILTER_ENV_VAR: &str = "SURU_LOG";
/// Filter applied when `SURU_LOG` is unset or invalid.
const DEFAULT_FILTER: &str = "warn,suru=info";
/// Per-run Log files kept before the oldest are pruned.
const RETAINED_LOG_FILES: usize = 20;
/// Dependencies that log the payloads they carry at their verbose levels, and
/// the most verbose level each is let log at whatever `SURU_LOG` asks: rmcp
/// logs each Broker call whole, Answers included, at debug and trace and its
/// notifications at info, while its warnings and errors say what went wrong
/// without the payload.
const PAYLOAD_BEARING_TARGETS: [(&str, LevelFilter); 1] = [("rmcp", LevelFilter::WARN)];

/// The process's role, naming its Log file and stamped into its opening line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Client,
    Server,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// Keeps the background Log writer alive; dropping it flushes buffered lines.
pub struct LogGuard {
    _worker: WorkerGuard,
}

/// Routes this process's `tracing` output to a new per-run Log file,
/// filtered by the `SURU_LOG` env var (`EnvFilter` directives).
pub fn init(config: &RuntimeConfig, role: Role) -> Result<LogGuard> {
    init_with_filter_directives(config, role, std::env::var(FILTER_ENV_VAR).ok())
}

/// Routes this process's `tracing` output to a new per-run Log file, filtered
/// by `directives` as by `SURU_LOG` — injectable so tests need not touch
/// process env vars.
pub fn init_with_filter_directives(
    config: &RuntimeConfig,
    role: Role,
    directives: Option<String>,
) -> Result<LogGuard> {
    let log_dir = config.state_dir().join(LOG_DIR);
    fs::create_dir_all(&log_dir).with_context(|| format!("create Log directory {log_dir:?}"))?;
    protect_current_user_directory(&log_dir)?;

    let path = log_dir.join(log_file_name(
        role,
        time::OffsetDateTime::now_utc(),
        std::process::id(),
    ));
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("open Log file {path:?}"))?;
    protect_current_user_file(&path)?;
    prune_stale_log_files(&log_dir, RETAINED_LOG_FILES);

    let (writer, worker) = tracing_appender::non_blocking(file);
    let (filter, invalid_directives) = filter_from(directives.as_deref());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(filter.and(payload_bound())),
        )
        .try_init()
        .map_err(|error| anyhow!("initialize logging: {error}"))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        channel = config.channel(),
        pid = std::process::id(),
        role = role.as_str(),
        "logging initialized"
    );
    if let Some(directives) = invalid_directives {
        tracing::warn!(
            "ignoring invalid {FILTER_ENV_VAR} filter {directives:?}; using {DEFAULT_FILTER:?}"
        );
    }
    Ok(LogGuard { _worker: worker })
}

fn filter_from(directives: Option<&str>) -> (EnvFilter, Option<String>) {
    match directives {
        Some(directives) if !directives.is_empty() => match EnvFilter::try_new(directives) {
            Ok(filter) => (filter, None),
            Err(_) => (EnvFilter::new(DEFAULT_FILTER), Some(directives.to_owned())),
        },
        _ => (EnvFilter::new(DEFAULT_FILTER), None),
    }
}

/// The bound no `SURU_LOG` directive lifts: every target logs as its own
/// directives say, but [`PAYLOAD_BEARING_TARGETS`] never more verbosely than
/// they allow. It stands beside the directives rather than among them, since
/// among them a more specific directive — `rmcp::service=trace` — would win.
fn payload_bound() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_targets(PAYLOAD_BEARING_TARGETS)
}

fn log_file_name(role: Role, at: time::OffsetDateTime, pid: u32) -> String {
    const TIMESTAMP: &[time::format_description::BorrowedFormatItem<'_>] =
        time::macros::format_description!("[year][month][day]T[hour][minute][second]");
    let timestamp = at.format(TIMESTAMP).expect("format Log file timestamp");
    format!("{timestamp}-{}-{pid}.log", role.as_str())
}

/// Best-effort removal of the oldest Log files beyond `keep`; the timestamped
/// names make lexicographic order chronological.
fn prune_stale_log_files(log_dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log") && path.is_file())
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    for path in files.drain(..files.len() - keep) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_names_sort_chronologically_and_carry_role_and_pid() {
        let first = log_file_name(
            Role::Server,
            time::macros::datetime!(2026-08-22 09:59:59 UTC),
            41,
        );
        let second = log_file_name(
            Role::Client,
            time::macros::datetime!(2026-08-22 10:00:00 UTC),
            42,
        );
        assert_eq!(first, "20260822T095959-server-41.log");
        assert_eq!(second, "20260822T100000-client-42.log");
        assert!(first < second);
    }

    #[test]
    fn pruning_removes_only_the_oldest_log_files_beyond_the_cap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        for name in [
            "20260822T100000-client-1.log",
            "20260822T100001-client-2.log",
            "20260822T100002-server-3.log",
            "notes.txt",
        ] {
            fs::write(dir.path().join(name), b"x").expect("write file");
        }
        prune_stale_log_files(dir.path(), 2);
        let mut remaining = fs::read_dir(dir.path())
            .expect("list dir")
            .flatten()
            .map(|entry| entry.file_name().into_string().expect("file name"))
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(
            remaining,
            [
                "20260822T100001-client-2.log",
                "20260822T100002-server-3.log",
                "notes.txt",
            ]
        );
    }

    #[test]
    fn invalid_filter_directives_fall_back_to_the_default() {
        let (_, invalid) = filter_from(Some("not a === filter"));
        assert_eq!(invalid.as_deref(), Some("not a === filter"));
        let (_, invalid) = filter_from(Some("suru=debug"));
        assert_eq!(invalid, None);
        let (_, invalid) = filter_from(None);
        assert_eq!(invalid, None);
    }

    /// However verbose `SURU_LOG` asks the Log to be — at any level, and naming
    /// the dependency outright — a dependency that logs the payloads it carries
    /// is held to its warnings and errors, while Suru's own lines are written
    /// as verbosely as asked.
    #[test]
    fn a_dependency_logging_payloads_is_held_to_warnings_whatever_the_filter_asks() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let config = RuntimeConfig::new(dir.path(), "logtest").expect("build runtime config");
        let guard = init_with_filter_directives(
            &config,
            Role::Server,
            Some("trace,rmcp=trace,rmcp::service=trace".to_owned()),
        )
        .expect("initialize logging");
        tracing::trace!(target: "rmcp::service", evt = "tok-trace-payload", "new event");
        tracing::debug!(target: "rmcp::service", request = "tok-debug-payload", "received request");
        tracing::info!(target: "rmcp::service", notification = "tok-info-payload", "received");
        tracing::warn!(target: "rmcp::service", "response error kept");
        tracing::trace!(marker = "suru-trace", "Suru's own trace line");
        drop(guard);
        let log_dir = config.state_dir().join(LOG_DIR);
        let contents = fs::read_dir(&log_dir)
            .expect("list Log dir")
            .flatten()
            .map(|entry| fs::read_to_string(entry.path()).expect("read Log file"))
            .collect::<String>();
        for payload in ["tok-trace-payload", "tok-debug-payload", "tok-info-payload"] {
            assert!(!contents.contains(payload), "{payload} reached the Log");
        }
        assert!(contents.contains("response error kept"), "{contents}");
        assert!(contents.contains("Suru's own trace line"), "{contents}");
    }

    #[test]
    fn init_writes_structured_lines_into_a_per_run_state_dir_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let config = RuntimeConfig::new(dir.path(), "logtest").expect("build runtime config");
        let guard =
            init_with_filter_directives(&config, Role::Client, None).expect("initialize logging");
        tracing::info!(marker = "init-test", "hello from the logging test");
        drop(guard);
        let log_dir = config.state_dir().join(LOG_DIR);
        let entries = fs::read_dir(&log_dir)
            .expect("list Log dir")
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        let contents = fs::read_to_string(entries[0].path()).expect("read Log file");
        assert!(contents.contains("logging initialized"));
        assert!(contents.contains("hello from the logging test"));
        assert!(contents.contains("marker=\"init-test\""));
    }
}
