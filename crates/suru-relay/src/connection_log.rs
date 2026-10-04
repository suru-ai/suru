//! The connection log: one line for each connection the Relay joins, so its
//! operator can answer who connected what to what — and never what was
//! carried, which the Relay cannot read (ADR-0045).
//!
//! Each line is one JSON object, written whole once its joined connection
//! ends, however it ends: either side closing, either side not taking in
//! what is carried to it, or the Relay stopping. The log goes to standard
//! output unless the Relay is configured otherwise, apart from the Relay's
//! diagnostic log on standard error, and the Relay keeps nothing of it in its
//! records.
//!
//! A thread of its own writes the log, so a reader that stops taking it in
//! holds up nothing else. The Relay never carries a connection it could not
//! record, nor drops a line unsaid. Each join is given room for its line
//! before it is asked, and a join the log has no room for — its reader having
//! fallen as far behind as [`crate::RelayConfig::with_connection_log_capacity`]
//! lets it — is refused until it catches up. A line the log cannot write at
//! all stops it for good, so nothing more follows a line left part-written:
//! the failure is reported on the diagnostic log, where that line and every
//! one the log owes after it go instead, and the Relay joins no more
//! connections until it is restarted. A stopping Relay waits for the log to
//! write what it owes no longer than
//! [`crate::RelayConfig::with_drain_timeout`] lets it, and says how many
//! lines it gave up.
//!
//! A line reads:
//!
//! ```json
//! {"event":"joined_connection","start":"2026-10-04T09:30:00.000Z","end":"2026-10-04T09:41:12.345Z","account":{"id":1,"provider":"github","subject":"583231","username":"octocat"},"joining":{"fingerprint":"9f86…","hostname":"laptop","address":"203.0.113.7","bytes_sent":1832},"serving":{"fingerprint":"60303…","hostname":"workstation","address":"198.51.100.2","bytes_sent":90211}}
//! ```
//!
//! `account` is the Account the join was made under: the Relay's own id for
//! it, and the identity that logs in as it — its provider, the provider's
//! stable id for it, numeric at GitHub, and its username there. `joining` is
//! the Server that asked to be joined and `serving` the one it was joined to,
//! which Serves through the Relay. Each is named by its identity key's
//! fingerprint, the hostname it reported as it logged in, and the network
//! address of the connection the Relay carried the join on for it — believed
//! from a reverse proxy only as [`crate::TrustedProxy`] says — with the bytes
//! it sent that the Relay passed on to the other: those of each frame the
//! Relay wrote out to the other's connection whole. A frame cut off part-way,
//! as the join is given up, reaches the other Server as no frame at all, and
//! is not counted. `start` is when the join was made and `end` when the Relay
//! had let go of all it carried, in UTC to the millisecond. `event` names what the line records, so a reader can tell it
//! from any other kind of line the log may come to hold.

use std::{
    io::Write,
    net::IpAddr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::Context;
use serde::Serialize;
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};
use tokio::sync::{mpsc, oneshot};

use crate::{
    Clock,
    store::{Account, Login, Parties},
};

/// How a line's times are written: RFC 3339, in UTC, to the millisecond.
const TIMESTAMP: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// What the Relay writes its connection log to, a whole line at a time.
pub(crate) type Writer = Arc<Mutex<dyn Write + Send>>;

/// The connection log a Relay writes to.
pub(crate) struct ConnectionLog {
    /// The lines handed to the log's writer, and the room held for those
    /// owed by joins still carried.
    lines: mpsc::Sender<String>,
    clock: Clock,
    /// Set once a line could not be written, from when the log writes — and
    /// so the Relay joins — nothing more.
    failed: Arc<AtomicBool>,
    /// How many lines the writer has been handed and has yet to write.
    unwritten: Arc<AtomicUsize>,
}

/// The connection log's writer, a thread of its own, until it has written
/// every line it is handed.
pub(crate) struct Writing {
    done: oneshot::Receiver<()>,
    unwritten: Arc<AtomicUsize>,
}

/// Room held in the connection log for the line one join is owed, from
/// before the join is asked until it ends.
pub(crate) struct Room(mpsc::OwnedPermit<String>);

impl ConnectionLog {
    /// Starts a log written to `writer` by a thread of its own, owing at most
    /// `capacity` lines at once — those of joins carried, and those of joins
    /// ended that are yet to be written — its times read from `clock`.
    pub(crate) fn start(
        writer: Writer,
        capacity: usize,
        clock: Clock,
    ) -> anyhow::Result<(Self, Writing)> {
        let (lines, handed) = mpsc::channel(capacity.max(1));
        let failed = Arc::new(AtomicBool::new(false));
        let unwritten = Arc::new(AtomicUsize::new(0));
        let (done, finished) = oneshot::channel::<()>();
        // The writer reports to the diagnostic log the Relay was started
        // under.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name("connection-log".to_owned())
            .spawn({
                let (failed, unwritten) = (failed.clone(), unwritten.clone());
                move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        write_lines(&writer, handed, &failed, &unwritten);
                    });
                    drop(writer);
                    drop(done);
                }
            })
            .context("start the connection log's writer")?;
        Ok((
            Self {
                lines,
                clock,
                failed,
                unwritten: unwritten.clone(),
            },
            Writing {
                done: finished,
                unwritten,
            },
        ))
    }

    /// Room for the line of one more join, where the log has it: it has not
    /// failed, and owes fewer lines than it may.
    pub(crate) fn room(&self) -> Option<Room> {
        if self.failed.load(Ordering::Acquire) {
            return None;
        }
        self.lines.clone().try_reserve_owned().ok().map(Room)
    }

    /// Begins the line owed, in `room`, for a join made between `parties`,
    /// the joining Server's connection coming from `joining` and the serving
    /// one's from `serving`: it is handed to the writer as what this answers
    /// drops.
    pub(crate) fn begin(
        &self,
        room: Room,
        parties: Parties,
        joining: IpAddr,
        serving: IpAddr,
    ) -> Entry<'_> {
        Entry {
            log: self,
            room: Some(room),
            start: self.clock.now(),
            account: parties.account,
            joining: Party::new(parties.joining, joining),
            serving: Party::new(parties.serving, serving),
        }
    }
}

impl Writing {
    /// Waits for the writer to write every line it was handed, once the
    /// Relay has let go of the log, for no longer than `timeout`: past it the
    /// lines still unwritten are given up, and the diagnostic log says how
    /// many.
    pub(crate) async fn finish(self, timeout: Duration) {
        if tokio::time::timeout(timeout, self.done).await.is_err() {
            tracing::error!(
                "the connection log's reader took in nothing more for {timeout:?}, so the Relay \
                 stopped with {} lines of it unwritten",
                self.unwritten.load(Ordering::Acquire)
            );
        }
    }
}

/// Writes each line `handed` brings to `writer`, whole, until the Relay lets
/// go of the log. A line that cannot be written stops the log: it and every
/// line after it go to the diagnostic log instead.
fn write_lines(
    writer: &Writer,
    mut handed: mpsc::Receiver<String>,
    failed: &AtomicBool,
    unwritten: &AtomicUsize,
) {
    while let Some(line) = handed.blocking_recv() {
        if !failed.load(Ordering::Acquire) {
            let written = {
                let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                writer
                    .write_all(line.as_bytes())
                    .and_then(|()| writer.flush())
            };
            match written {
                Ok(()) => {
                    unwritten.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    tracing::error!(
                        "the connection log could not be written, so this Relay joins no more \
                         connections until it is restarted, and the lines the log owes go here \
                         instead: {error}"
                    );
                }
            }
        }
        tracing::error!(
            line = %line.trim_end(),
            "a joined connection the connection log could not record"
        );
        unwritten.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The line one joined connection is owed, handed to the log's writer once
/// as this drops: when the Relay has let go of all the connection carried,
/// however that comes about — the Relay stopping, and so dropping what
/// carries it, among them.
pub(crate) struct Entry<'log> {
    log: &'log ConnectionLog,
    room: Option<Room>,
    start: SystemTime,
    account: Account,
    pub(crate) joining: Party,
    pub(crate) serving: Party,
}

/// One of the two Servers of a joined connection, as its line names it.
pub(crate) struct Party {
    login: Login,
    address: IpAddr,
    /// The bytes of each frame the Server sent that the Relay passed on to
    /// the other, written out to the other's connection whole.
    sent: AtomicU64,
    /// The bytes of the frame the Server sent that the Relay has queued on
    /// the other's connection and has yet to write out whole.
    queued: AtomicU64,
}

impl Party {
    fn new(login: Login, address: IpAddr) -> Self {
        Self {
            login,
            address,
            sent: AtomicU64::new(0),
            queued: AtomicU64::new(0),
        }
    }

    /// The frame of `length` bytes the Server sent is queued on the other's
    /// connection.
    pub(crate) fn queue(&self, length: u64) {
        self.queued.store(length, Ordering::Relaxed);
    }

    /// Whatever the Server sent that was queued on the other's connection
    /// has been written out of it whole.
    pub(crate) fn pass_on(&self) {
        self.sent
            .fetch_add(self.queued.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    }

    fn line(&self) -> ServerLine<'_> {
        ServerLine {
            fingerprint: &self.login.fingerprint,
            hostname: &self.login.hostname,
            address: self.address,
            bytes_sent: self.sent.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Entry<'_> {
    fn drop(&mut self) {
        let mut line = serde_json::to_string(&Line {
            event: "joined_connection",
            start: timestamp(self.start),
            end: timestamp(self.log.clock.now()),
            account: AccountLine {
                id: self.account.id,
                provider: &self.account.provider,
                subject: &self.account.subject,
                username: &self.account.username,
            },
            joining: self.joining.line(),
            serving: self.serving.line(),
        })
        .expect("a connection log line always encodes");
        line.push('\n');
        if let Some(Room(room)) = self.room.take() {
            self.log.unwritten.fetch_add(1, Ordering::AcqRel);
            room.send(line);
        }
    }
}

/// One line of the log, its fields in the order they are written.
#[derive(Serialize)]
struct Line<'entry> {
    event: &'static str,
    start: String,
    end: String,
    account: AccountLine<'entry>,
    joining: ServerLine<'entry>,
    serving: ServerLine<'entry>,
}

#[derive(Serialize)]
struct AccountLine<'entry> {
    id: i64,
    provider: &'entry str,
    subject: &'entry str,
    username: &'entry str,
}

#[derive(Serialize)]
struct ServerLine<'entry> {
    fingerprint: &'entry str,
    hostname: &'entry str,
    address: IpAddr,
    bytes_sent: u64,
}

fn timestamp(time: SystemTime) -> String {
    OffsetDateTime::from(time)
        .format(TIMESTAMP)
        .unwrap_or_default()
}
