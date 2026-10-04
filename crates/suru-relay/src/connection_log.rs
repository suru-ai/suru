//! The connection log: one line for each connection the Relay joins, so its
//! operator can answer who connected what to what — and never what was
//! carried, which the Relay cannot read (ADR-0045).
//!
//! Each line is one JSON object, written whole once its joined connection
//! ends, however it ends: either side closing, either side not taking in
//! what is carried to it, or the Relay stopping. The log goes to standard
//! output unless the Relay is configured otherwise, apart from the Relay's
//! diagnostic log on standard error, and the Relay keeps nothing of it in its
//! records. A line reads:
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
//! it sent that the Relay carried to the other. `start` is when the join was
//! made and `end` when the Relay stopped carrying it, in UTC to the
//! millisecond. `event` names what the line records, so a reader can tell it
//! from any other kind of line the log may come to hold.

use std::{
    io::Write,
    net::IpAddr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use serde::Serialize;
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};

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
    writer: Writer,
    clock: Clock,
}

impl ConnectionLog {
    /// A log written to `writer`, its times read from `clock`.
    pub(crate) fn new(writer: Writer, clock: Clock) -> Self {
        Self { writer, clock }
    }

    /// Begins the line owed for a join made between `parties`, the joining
    /// Server's connection coming from `joining` and the serving one's from
    /// `serving`: it is written as what this answers drops.
    pub(crate) fn begin(&self, parties: Parties, joining: IpAddr, serving: IpAddr) -> Entry<'_> {
        Entry {
            log: self,
            start: self.clock.now(),
            account: parties.account,
            joining: Party::new(parties.joining, joining),
            serving: Party::new(parties.serving, serving),
        }
    }

    fn write(&self, line: &Line<'_>) {
        let mut text = serde_json::to_vec(line).expect("a connection log line always encodes");
        text.push(b'\n');
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = writer.write_all(&text).and_then(|()| writer.flush()) {
            tracing::error!("the connection log could not be written: {error}");
        }
    }
}

/// The line one joined connection is owed, written once as this drops: when
/// the Relay stops carrying the connection, however that comes about — the
/// Relay stopping, and so dropping what carries it, among them.
pub(crate) struct Entry<'log> {
    log: &'log ConnectionLog,
    start: SystemTime,
    account: Account,
    pub(crate) joining: Party,
    pub(crate) serving: Party,
}

/// One of the two Servers of a joined connection, as its line names it.
pub(crate) struct Party {
    login: Login,
    address: IpAddr,
    /// The bytes the Server sent that the Relay carried on to the other.
    pub(crate) sent: AtomicU64,
}

impl Party {
    fn new(login: Login, address: IpAddr) -> Self {
        Self {
            login,
            address,
            sent: AtomicU64::new(0),
        }
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
        self.log.write(&Line {
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
        });
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
