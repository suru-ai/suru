//! Reads of directories that may never answer — a network filesystem gone
//! silent, say — each made on a thread of its own with a runtime of its own,
//! apart from the Server's executor and its blocking pool, so a read that
//! never returns holds that one thread and keeps no runtime from shutting
//! down. At most one read of a path is outstanding at once, so a path that
//! never answers holds one thread however often it is asked for again, and
//! no more reads are outstanding at once than a limit allows. Reclaim reads
//! the Workspace Registry's entries this way, discovery each Workspace Root
//! and the directories directly within it, and every Workspace listing where
//! each entry is presented.

use std::{
    collections::HashSet,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Why a path was not read.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum NotRead {
    /// A read of it begun earlier is still outstanding.
    Outstanding,
    /// As many reads as the limit allows are outstanding.
    AtLimit,
    /// It could not be read apart: no thread or runtime could be made for
    /// it, or its reader ended without answering.
    Failed(String),
}

impl fmt::Display for NotRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Outstanding => {
                formatter.write_str("a read of it begun earlier is still outstanding")
            }
            Self::AtLimit => {
                formatter.write_str("as many reads as are allowed at once are outstanding")
            }
            Self::Failed(why) => formatter.write_str(why),
        }
    }
}

/// Makes reads apart, each path one read at a time.
pub(super) struct ReadsApart {
    /// What each reader thread is named.
    name: &'static str,
    /// The most reads outstanding at once.
    limit: usize,
    /// The paths whose reads have not yet ended.
    outstanding: Arc<Mutex<HashSet<PathBuf>>>,
}

impl ReadsApart {
    pub(super) fn new(name: &'static str, limit: usize) -> Self {
        Self {
            name,
            limit,
            outstanding: Arc::default(),
        }
    }

    /// Reads `path` with `read`, run to its end on a thread and a runtime of
    /// its own, answering what it read — unless a read of `path` is still
    /// outstanding, or as many reads as the limit allows are. A caller that
    /// stops waiting leaves the read to end on its own thread, and `path`
    /// outstanding until it does.
    pub(super) async fn read<T, Reading>(
        &self,
        path: &Path,
        read: impl FnOnce() -> Reading + Send + 'static,
    ) -> Result<T, NotRead>
    where
        T: Send + 'static,
        Reading: Future<Output = T>,
    {
        {
            let mut outstanding = self.outstanding.lock().unwrap();
            if outstanding.contains(path) {
                return Err(NotRead::Outstanding);
            }
            if outstanding.len() >= self.limit {
                return Err(NotRead::AtLimit);
            }
            outstanding.insert(path.to_owned());
        }
        let ended = Ended {
            outstanding: self.outstanding.clone(),
            path: path.to_owned(),
        };
        let (answer, answered) = tokio::sync::oneshot::channel();
        // A thread that cannot be made drops what it would have run, and
        // with it `ended`, so the path is outstanding no longer; one that
        // panics drops it as it unwinds.
        std::thread::Builder::new()
            .name(self.name.to_owned())
            .spawn(move || {
                let read = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|runtime| runtime.block_on(read()))
                    .map_err(|error| {
                        NotRead::Failed(format!("no runtime could be made to read it on: {error}"))
                    });
                // The read has ended: the path is let go before the read is
                // answered, so whoever it answers may read it again at once.
                drop(ended);
                let _ = answer.send(read);
            })
            .map_err(|error| {
                NotRead::Failed(format!("no thread could be made to read it on: {error}"))
            })?;
        answered.await.unwrap_or_else(|_| {
            Err(NotRead::Failed(
                "its reader ended without answering".to_owned(),
            ))
        })
    }
}

/// One path's read, outstanding until the thread reading it lets it go,
/// however that thread ends.
struct Ended {
    outstanding: Arc<Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for Ended {
    fn drop(&mut self) {
        if let Ok(mut outstanding) = self.outstanding.lock() {
            outstanding.remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Condvar,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;

    /// Holds every thread that waits on it until it is opened, and opens
    /// once dropped, so no reader is left held when a test ends.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    struct OpensOnDrop(Arc<Gate>);

    impl Drop for OpensOnDrop {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// How long a test waits for something it knows will happen.
    const SURELY: Duration = Duration::from_secs(10);

    /// A read that counts itself begun, then waits on `gate`.
    fn held(
        gate: &Arc<Gate>,
        begun: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> std::future::Ready<&'static str> + Send + 'static {
        let gate = gate.clone();
        let begun = begun.clone();
        move || {
            begun.fetch_add(1, Ordering::SeqCst);
            gate.wait();
            std::future::ready("read")
        }
    }

    /// Waits until `begun` counts `count` reads begun.
    async fn until_begun(begun: &AtomicUsize, count: usize) {
        tokio::time::timeout(SURELY, async {
            while begun.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reads begin");
    }

    #[tokio::test]
    async fn a_path_is_read_once_at_a_time_and_passed_over_meanwhile() {
        let reads = Arc::new(ReadsApart::new("test-read", 8));
        let gate = Arc::new(Gate::default());
        let held_open = OpensOnDrop(gate.clone());
        let begun = Arc::new(AtomicUsize::new(0));
        let path = PathBuf::from("stalled");
        let first = tokio::spawn({
            let reads = reads.clone();
            let read = held(&gate, &begun);
            let path = path.clone();
            async move { reads.read(&path, read).await }
        });
        until_begun(&begun, 1).await;

        for _ in 0..3 {
            assert_eq!(
                reads.read(&path, held(&gate, &begun)).await,
                Err(NotRead::Outstanding),
                "a path whose read is outstanding is passed over"
            );
        }
        assert_eq!(begun.load(Ordering::SeqCst), 1, "and is read only once");

        drop(held_open);
        assert_eq!(
            tokio::time::timeout(SURELY, first)
                .await
                .expect("the read ends once the path answers")
                .expect("the read runs to its end"),
            Ok("read")
        );
        assert_eq!(
            tokio::time::timeout(SURELY, reads.read(&path, held(&gate, &begun)))
                .await
                .expect("a path that answers is read again"),
            Ok("read")
        );
    }

    /// A read is outstanding no longer once it is answered, so whoever it
    /// answered reads the same path again at once — as a listing reads the
    /// directories the one before it read — and is never passed over.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_path_answered_is_read_again_at_once() {
        let reads = ReadsApart::new("test-read", 8);
        let path = Path::new("answered");
        for _ in 0..2_000 {
            assert_eq!(
                reads.read(path, || std::future::ready("read")).await,
                Ok("read"),
                "a path whose read was answered is outstanding no longer"
            );
        }
    }

    #[tokio::test]
    async fn no_more_reads_are_outstanding_at_once_than_the_limit() {
        let reads = Arc::new(ReadsApart::new("test-read", 2));
        let gate = Arc::new(Gate::default());
        let _held_open = OpensOnDrop(gate.clone());
        let begun = Arc::new(AtomicUsize::new(0));
        for path in ["one", "two"] {
            let reads = reads.clone();
            let read = held(&gate, &begun);
            tokio::spawn(async move { reads.read(Path::new(path), read).await });
        }
        until_begun(&begun, 2).await;

        assert_eq!(
            reads.read(Path::new("three"), held(&gate, &begun)).await,
            Err(NotRead::AtLimit)
        );
        assert_eq!(begun.load(Ordering::SeqCst), 2);
    }

    /// A read that never answers is held on a thread of its own, so the
    /// runtime that asked for it still runs its blocking work — here on a
    /// blocking pool of one thread — and shuts down without waiting on it.
    #[test]
    fn a_read_that_never_answers_holds_neither_the_runtime_nor_its_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("build a runtime");
        let gate = Arc::new(Gate::default());
        let held_open = OpensOnDrop(gate.clone());
        let begun = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(ReadsApart::new("test-read", 8));
        runtime.block_on(async {
            let stalled = {
                let reads = reads.clone();
                let read = held(&gate, &begun);
                tokio::spawn(async move { reads.read(Path::new("stalled"), read).await })
            };
            until_begun(&begun, 1).await;
            assert_eq!(
                tokio::time::timeout(SURELY, tokio::task::spawn_blocking(|| "blocking work"))
                    .await
                    .expect("the blocking pool is free")
                    .expect("the blocking work runs"),
                "blocking work"
            );
            stalled.abort();
        });
        drop(runtime);
        drop(held_open);
    }
}
