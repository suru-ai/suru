//! Which Relay runs on a database: one at a time. A running Relay holds a
//! lock on a file beside its records — the database's own name with `.lock`
//! after it — which the operating system lets go the moment the Relay stops,
//! however it stops. A second Relay started on the same records is refused,
//! and the operator's command line looks at the lock to tell whether a Relay
//! is there to cut what it removes, without waiting on one that is not.

use std::{
    fs::{File, OpenOptions, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

/// How long a Relay starting goes on trying for the lock beside its records
/// before it takes another Relay to hold it: the operator's command line,
/// looking at it, holds it for no more than a moment.
const LOCK_PATIENCE: Duration = Duration::from_secs(1);

/// The lock beside the records at `database`.
fn lock_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Holds the lock beside the records at `database` for as long as what this
/// answers is kept, refusing where another Relay holds it.
pub(crate) async fn run_on(database: &Path) -> Result<File> {
    let path = lock_path(database);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open the lock a running Relay holds, {path:?}"))?;
    let deadline = tokio::time::Instant::now() + LOCK_PATIENCE;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(TryLockError::WouldBlock) => bail!(
                "another Relay is running on the records at {database:?}, which are run by one \
                 Relay at a time"
            ),
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("take the lock a running Relay holds, {path:?}"));
            }
        }
    }
}

/// Whether a Relay may be running on the records at `database`: none is
/// only where nothing holds the lock beside them, or it is not there.
pub(crate) fn may_be_running(database: &Path) -> bool {
    let lock = match OpenOptions::new().write(true).open(lock_path(database)) {
        Ok(lock) => lock,
        Err(error) => return error.kind() != ErrorKind::NotFound,
    };
    match lock.try_lock_shared() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_relay_runs_on_a_database_at_a_time_and_its_lock_says_so_until_it_stops() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("relay.db");
        assert!(!may_be_running(&database), "no lock is there");

        let running = run_on(&database).await.unwrap();
        assert!(may_be_running(&database));
        let refused = run_on(&database).await.unwrap_err().to_string();
        assert!(refused.contains("another Relay is running"), "{refused}");

        drop(running);
        assert!(!may_be_running(&database), "a lock nothing holds");
        let _running = run_on(&database).await.unwrap();
    }
}
