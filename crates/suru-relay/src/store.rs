//! The Relay's records: its Accounts, the identities that log in as them,
//! and the Logins that stand under them, kept in SQLite. A Login stands while
//! its Account has not lapsed, and — where the Relay requires a fresh login
//! every so often — while the Account has been logged in as since.
//!
//! These are held to ADR-0047: a Relay upgraded in place keeps them, so the
//! schema moves forward by migration and is never replaced.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use diesel::{
    OptionalExtension, SqliteConnection, connection::SimpleConnection, prelude::*, upsert::excluded,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

use crate::identity::Identity;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// The longest one step of the store waits on SQLite's own locks — another
/// process holding the database, such as the operator's command line —
/// before it fails rather than waiting on.
const BUSY_TIMEOUT_MS: u32 = 5000;

diesel::table! {
    accounts (id) {
        id -> BigInt,
        created_at -> BigInt,
        logged_in_at -> BigInt,
        lapsed_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    identities (provider, subject) {
        provider -> Text,
        subject -> Text,
        username -> Text,
        account_id -> BigInt,
    }
}

diesel::table! {
    logins (server_key) {
        server_key -> Binary,
        fingerprint -> Text,
        account_id -> BigInt,
        hostname -> Text,
        formed_at -> BigInt,
    }
}

diesel::joinable!(identities -> accounts (account_id));
diesel::joinable!(logins -> accounts (account_id));
diesel::allow_tables_to_appear_in_same_query!(accounts, identities, logins);

/// An Account as the Relay keeps it, with the one identity that logs in as
/// it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub id: i64,
    /// The identity provider the Account's identity is at.
    pub provider: String,
    /// The provider's stable id for that identity, by which it is known.
    pub subject: String,
    /// The name the identity had at its latest login: a label, never a key.
    pub username: String,
    /// Whether the Account has lapsed: every Login under it refused until one
    /// of its Servers logs in afresh.
    pub lapsed: bool,
}

/// A Login as the Relay keeps it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Login {
    /// The Account the Login stands under.
    pub account: i64,
    /// The fingerprint of the identity key the Login is tied to.
    pub fingerprint: String,
    /// The hostname the Server reported as it logged in.
    pub hostname: String,
    /// When the Login was formed.
    pub formed_at: SystemTime,
}

/// An Account a Login stands under, to be checked against the Relay's
/// admission rules, with the identity that logs in as it.
pub(crate) struct StandingAccount {
    pub(crate) id: i64,
    pub(crate) provider: String,
    pub(crate) identity: Identity,
    /// When it was last logged in as, in seconds since the Unix epoch.
    pub(crate) logged_in_at: i64,
}

impl StandingAccount {
    /// Whether it has not been logged in as since `fresh_since`, so that a
    /// Relay requiring a fresh login since then lapses it.
    pub(crate) fn is_due(&self, fresh_since: Option<SystemTime>) -> bool {
        self.logged_in_at <= since(fresh_since)
    }
}

/// A login recorded: the Account it stands under, as the Server is told of
/// it, and by its id, and the Account the key's Login stood under before, by
/// its id, where the key held one.
pub(crate) struct Recorded {
    pub(crate) account: suru_relay_protocol::Account,
    pub(crate) id: i64,
    pub(crate) previous: Option<i64>,
}

impl Recorded {
    /// Whether the key's Login has moved to another Account than the one it
    /// stood under before.
    pub(crate) fn moved(&self) -> bool {
        self.previous.is_some_and(|previous| previous != self.id)
    }
}

/// The two Logins a join is made between, and the Account both stand under.
pub(crate) struct Parties {
    pub(crate) account: Account,
    /// The Login of the Server that asked for the join.
    pub(crate) joining: Login,
    /// The Login of the Server the join was asked of.
    pub(crate) serving: Login,
}

/// The Relay's SQLite records.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<SqliteConnection>>,
}

impl Store {
    /// Opens the records at `path`, creating them where there are none and
    /// carrying older ones forward.
    pub fn open(path: &Path) -> Result<Self> {
        let url = path
            .to_str()
            .with_context(|| format!("the Relay's database path {path:?} is not UTF-8"))?;
        let mut connection = SqliteConnection::establish(url)
            .with_context(|| format!("open the Relay's database {path:?}"))?;
        connection
            .batch_execute(&format!(
                "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}; PRAGMA journal_mode = WAL; \
                 PRAGMA foreign_keys = ON;"
            ))
            .context("configure the Relay's database")?;
        connection
            .run_pending_migrations(MIGRATIONS)
            .map_err(|error| anyhow::anyhow!("migrate the Relay's database: {error}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// The Account the Login tied to `server_key` stands under, by its
    /// provider and username, where there is such a Login and it stands,
    /// fresh enough where a login is required since `fresh_since`.
    pub(crate) async fn standing(
        &self,
        server_key: &[u8],
        fresh_since: Option<SystemTime>,
    ) -> Result<Option<suru_relay_protocol::Account>> {
        let server_key = server_key.to_vec();
        self.run(move |connection| {
            logins::table
                .inner_join(accounts::table)
                .inner_join(identities::table.on(identities::account_id.eq(logins::account_id)))
                .filter(logins::server_key.eq(server_key))
                .filter(stands(since(fresh_since)))
                .select((identities::provider, identities::username))
                .first::<(String, String)>(connection)
                .optional()
                .map(|found| {
                    found.map(|(provider, username)| suru_relay_protocol::Account {
                        provider,
                        username,
                    })
                })
                .context("read a Server's Login")
        })
        .await
    }

    /// The Account the Login tied to `server_key` stands under, by its id,
    /// where there is such a Login and it stands, fresh enough where a login
    /// is required since `fresh_since`.
    pub(crate) async fn account_of(
        &self,
        server_key: &[u8],
        fresh_since: Option<SystemTime>,
    ) -> Result<Option<i64>> {
        let server_key = server_key.to_vec();
        self.run(move |connection| {
            logins::table
                .inner_join(accounts::table)
                .filter(logins::server_key.eq(server_key))
                .filter(stands(since(fresh_since)))
                .select(logins::account_id)
                .first::<i64>(connection)
                .optional()
                .context("read a Server's Account")
        })
        .await
    }

    /// The Logins tied to `joining` and to `serving`, with the Account
    /// whose id is `account`, where both stand under it — fresh enough
    /// where a login is required since `fresh_since`.
    pub(crate) async fn parties(
        &self,
        joining: &[u8],
        serving: &[u8],
        account: i64,
        fresh_since: Option<SystemTime>,
    ) -> Result<Option<Parties>> {
        let (joining, serving) = (joining.to_vec(), serving.to_vec());
        self.run(move |connection| {
            connection
                .transaction(|connection| {
                    let account_stands = accounts::table
                        .find(account)
                        .filter(stands(since(fresh_since)))
                        .count()
                        .get_result::<i64>(connection)?
                        > 0;
                    if !account_stands {
                        return Ok(None);
                    }
                    let standing = |connection: &mut SqliteConnection, key: Vec<u8>| {
                        logins::table
                            .find(key)
                            .filter(logins::account_id.eq(account))
                            .select(LOGIN_COLUMNS)
                            .first::<LoginRow>(connection)
                            .optional()
                            .map(|row| row.map(login))
                    };
                    let (Some(joining), Some(serving)) = (
                        standing(connection, joining)?,
                        standing(connection, serving)?,
                    ) else {
                        return Ok(None);
                    };
                    let identity = identities::table
                        .filter(identities::account_id.eq(account))
                        .order((identities::provider, identities::subject))
                        .select((
                            identities::provider,
                            identities::subject,
                            identities::username,
                        ))
                        .first::<(String, String, String)>(connection)
                        .optional()?;
                    diesel::QueryResult::Ok(identity.map(|(provider, subject, username)| Parties {
                        account: Account {
                            id: account,
                            provider,
                            subject,
                            username,
                            lapsed: false,
                        },
                        joining,
                        serving,
                    }))
                })
                .context("read the Logins a join is made between")
        })
        .await
    }

    /// The Account the identity `subject`, at `provider`, answers to, by its
    /// id, where it answers to one.
    pub(crate) async fn account_answering(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<i64>> {
        let (provider, subject) = (provider.to_owned(), subject.to_owned());
        self.run(move |connection| {
            identities::table
                .find((provider, subject))
                .select(identities::account_id)
                .first::<i64>(connection)
                .optional()
                .context("read the Account an identity answers to")
        })
        .await
    }

    /// Records that `identity`, at `provider`, logged in for the Server whose
    /// key is `server_key`: finds the Account that identity answers to, or
    /// creates one, and ties a Login under it to the key, labelled with
    /// `hostname`. A Login the key already held is formed anew under it. The
    /// Account is logged in as afresh, which restores it where it had lapsed,
    /// and every Login under it with it.
    pub(crate) async fn record_login(
        &self,
        provider: &str,
        identity: Identity,
        server_key: &[u8],
        hostname: String,
        now: SystemTime,
    ) -> Result<Recorded> {
        let provider = provider.to_owned();
        let server_key = server_key.to_vec();
        let now = unix_seconds(now);
        self.run(move |connection| {
            connection
                .transaction(|connection| {
                    let previous = logins::table
                        .find(&server_key)
                        .select(logins::account_id)
                        .first::<i64>(connection)
                        .optional()?;
                    let known = identities::table
                        .find((&provider, &identity.subject))
                        .select(identities::account_id)
                        .first::<i64>(connection)
                        .optional()?;
                    let account = match known {
                        Some(account) => {
                            diesel::update(accounts::table.find(account))
                                .set((
                                    accounts::logged_in_at.eq(now),
                                    accounts::lapsed_at.eq(None::<i64>),
                                ))
                                .execute(connection)?;
                            account
                        }
                        None => diesel::insert_into(accounts::table)
                            .values((accounts::created_at.eq(now), accounts::logged_in_at.eq(now)))
                            .returning(accounts::id)
                            .get_result::<i64>(connection)?,
                    };
                    diesel::insert_into(identities::table)
                        .values((
                            identities::provider.eq(&provider),
                            identities::subject.eq(&identity.subject),
                            identities::username.eq(&identity.username),
                            identities::account_id.eq(account),
                        ))
                        .on_conflict((identities::provider, identities::subject))
                        .do_update()
                        .set(identities::username.eq(excluded(identities::username)))
                        .execute(connection)?;
                    diesel::insert_into(logins::table)
                        .values((
                            logins::server_key.eq(&server_key),
                            logins::fingerprint.eq(suru_relay_protocol::fingerprint(&server_key)),
                            logins::account_id.eq(account),
                            logins::hostname.eq(&hostname),
                            logins::formed_at.eq(now),
                        ))
                        .on_conflict(logins::server_key)
                        .do_update()
                        .set((
                            logins::account_id.eq(excluded(logins::account_id)),
                            logins::hostname.eq(excluded(logins::hostname)),
                            logins::formed_at.eq(excluded(logins::formed_at)),
                        ))
                        .execute(connection)?;
                    diesel::QueryResult::Ok((account, previous))
                })
                .context("record a Login")
                .map(|(id, previous)| Recorded {
                    account: suru_relay_protocol::Account {
                        provider,
                        username: identity.username,
                    },
                    id,
                    previous,
                })
        })
        .await
    }

    /// Every Account a Login stands under, with the identity that logs in as
    /// it, in the order they were made.
    pub(crate) async fn standing_accounts(&self) -> Result<Vec<StandingAccount>> {
        self.run(|connection| {
            let rows = accounts::table
                .inner_join(identities::table)
                .filter(accounts::lapsed_at.is_null())
                .filter(diesel::dsl::exists(
                    logins::table.filter(logins::account_id.eq(accounts::id)),
                ))
                .order((accounts::id, identities::provider, identities::subject))
                .select((
                    accounts::id,
                    identities::provider,
                    identities::subject,
                    identities::username,
                    accounts::logged_in_at,
                ))
                .load::<(i64, String, String, String, i64)>(connection)
                .context("list the Accounts Logins stand under")?;
            let mut standing = Vec::<StandingAccount>::with_capacity(rows.len());
            for (id, provider, subject, username, logged_in_at) in rows {
                // An Account is checked once, by the first identity that
                // logs in as it.
                if standing.last().is_none_or(|last| last.id != id) {
                    standing.push(StandingAccount {
                        id,
                        provider,
                        identity: Identity { subject, username },
                        logged_in_at,
                    });
                }
            }
            Ok(standing)
        })
        .await
    }

    /// Lapses the Account `account`, where it stands — and, where
    /// `unless_logged_in_since` is given, where it has not been logged in as
    /// since then: answers the identity keys of the Logins under it, which
    /// stand no longer, or `None` where it did not lapse. Nothing of it is
    /// forgotten.
    pub(crate) async fn lapse(
        &self,
        account: i64,
        now: SystemTime,
        unless_logged_in_since: Option<i64>,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        let now = unix_seconds(now);
        self.run(move |connection| {
            connection
                .transaction(|connection| {
                    let standing = accounts::table
                        .find(account)
                        .filter(accounts::lapsed_at.is_null())
                        .filter(
                            accounts::logged_in_at.le(unless_logged_in_since.unwrap_or(i64::MAX)),
                        );
                    let lapsed = diesel::update(standing)
                        .set(accounts::lapsed_at.eq(now))
                        .execute(connection)?;
                    if lapsed == 0 {
                        return Ok(None);
                    }
                    logins::table
                        .filter(logins::account_id.eq(account))
                        .select(logins::server_key)
                        .load::<Vec<u8>>(connection)
                        .map(Some)
                })
                .context("lapse an Account")
        })
        .await
    }

    /// Forgets the Login tied to `server_key`, answering whether there was
    /// one.
    pub(crate) async fn forget(&self, server_key: &[u8]) -> Result<bool> {
        let server_key = server_key.to_vec();
        self.run(move |connection| {
            diesel::delete(logins::table.find(server_key))
                .execute(connection)
                .map(|removed| removed > 0)
                .context("forget a Login")
        })
        .await
    }

    /// Every Account, in the order they were made.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        self.run(|connection| {
            identities::table
                .inner_join(accounts::table)
                .order(accounts::id)
                .select((
                    accounts::id,
                    identities::provider,
                    identities::subject,
                    identities::username,
                    accounts::lapsed_at,
                ))
                .load::<(i64, String, String, String, Option<i64>)>(connection)
                .map(|rows| {
                    rows.into_iter()
                        .map(|(id, provider, subject, username, lapsed_at)| Account {
                            id,
                            provider,
                            subject,
                            username,
                            lapsed: lapsed_at.is_some(),
                        })
                        .collect()
                })
                .context("list Accounts")
        })
        .await
    }

    /// Every Login, in the order they were formed.
    pub async fn logins(&self) -> Result<Vec<Login>> {
        self.run(|connection| {
            logins::table
                .order((logins::formed_at, logins::fingerprint))
                .select(LOGIN_COLUMNS)
                .load::<LoginRow>(connection)
                .map(|rows| rows.into_iter().map(login).collect())
                .context("list Logins")
        })
        .await
    }

    async fn run<T: Send + 'static>(
        &self,
        query: impl FnOnce(&mut SqliteConnection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .expect("the Relay's database lock is not poisoned");
            query(&mut connection)
        })
        .await
        .context("the Relay's database task ended")?
    }
}

/// The columns a [`Login`] is read from, as a [`LoginRow`].
const LOGIN_COLUMNS: (
    logins::account_id,
    logins::fingerprint,
    logins::hostname,
    logins::formed_at,
) = (
    logins::account_id,
    logins::fingerprint,
    logins::hostname,
    logins::formed_at,
);

type LoginRow = (i64, String, String, i64);

fn login((account, fingerprint, hostname, formed_at): LoginRow) -> Login {
    Login {
        account,
        fingerprint,
        hostname,
        formed_at: UNIX_EPOCH + Duration::from_secs(u64::try_from(formed_at).unwrap_or(0)),
    }
}

/// The Accounts that stand: not lapsed, and logged in as since `since`, in
/// seconds since the Unix epoch.
#[diesel::dsl::auto_type]
fn stands(since: i64) -> _ {
    accounts::lapsed_at
        .is_null()
        .and(accounts::logged_in_at.gt(since))
}

/// `fresh_since`, a time an Account must have been logged in as since, in
/// seconds since the Unix epoch: the earliest there are, where it need not.
fn since(fresh_since: Option<SystemTime>) -> i64 {
    fresh_since.map_or(i64::MIN, unix_seconds)
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(subject: &str, username: &str) -> Identity {
        Identity {
            subject: subject.to_owned(),
            username: username.to_owned(),
        }
    }

    #[tokio::test]
    async fn an_identity_is_one_account_however_its_name_changes_and_each_key_holds_one_login() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("relay.db")).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        store
            .record_login(
                "github",
                identity("17", "octo"),
                b"laptop",
                "laptop".into(),
                now,
            )
            .await
            .unwrap();
        let renamed = store
            .record_login(
                "github",
                identity("17", "octocat"),
                b"workstation",
                "workstation".into(),
                now,
            )
            .await
            .unwrap();
        assert_eq!(renamed.account.username, "octocat");
        store
            .record_login(
                "github",
                identity("99", "octo"),
                b"stranger",
                "box".into(),
                now,
            )
            .await
            .unwrap();

        let accounts = store.accounts().await.unwrap();
        assert_eq!(
            accounts.len(),
            2,
            "a name taken by another identity is another Account"
        );
        assert_eq!(
            (accounts[0].subject.as_str(), accounts[0].username.as_str()),
            ("17", "octocat")
        );
        assert_eq!(accounts[1].subject, "99");
        let logins = store.logins().await.unwrap();
        assert_eq!(logins.len(), 3);
        assert_eq!(
            logins
                .iter()
                .filter(|login| login.account == accounts[0].id)
                .count(),
            2
        );

        store
            .record_login(
                "github",
                identity("99", "octo"),
                b"laptop",
                "laptop again".into(),
                now,
            )
            .await
            .unwrap();
        let logins = store.logins().await.unwrap();
        assert_eq!(
            logins.len(),
            3,
            "a key logging in again forms its Login anew"
        );
        let laptop = logins
            .iter()
            .find(|login| login.fingerprint == suru_relay_protocol::fingerprint(b"laptop"))
            .unwrap();
        assert_eq!(laptop.account, accounts[1].id);
        assert_eq!(laptop.hostname, "laptop again");
        assert_eq!(
            store.standing(b"laptop", None).await.unwrap(),
            Some(suru_relay_protocol::Account {
                provider: "github".to_owned(),
                username: "octo".to_owned(),
            })
        );

        assert!(store.forget(b"laptop").await.unwrap());
        assert!(!store.forget(b"laptop").await.unwrap());
        assert_eq!(store.standing(b"laptop", None).await.unwrap(), None);
        assert_eq!(
            store.accounts().await.unwrap().len(),
            2,
            "forgetting a Login keeps its Account"
        );
    }

    #[tokio::test]
    async fn the_records_outlast_the_relay_that_wrote_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay.db");
        let formed_at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        Store::open(&path)
            .unwrap()
            .record_login(
                "github",
                identity("17", "octo"),
                b"key",
                "laptop".into(),
                formed_at,
            )
            .await
            .unwrap();

        let reopened = Store::open(&path).unwrap();
        let logins = reopened.logins().await.unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].formed_at, formed_at);
        assert!(reopened.standing(b"key", None).await.unwrap().is_some());
    }

    async fn account(store: &Store, subject: &str) -> Account {
        store
            .accounts()
            .await
            .unwrap()
            .into_iter()
            .find(|account| account.subject == subject)
            .unwrap()
    }

    #[tokio::test]
    async fn a_lapsed_account_refuses_its_logins_forgets_nothing_and_one_fresh_login_restores_them_all()
     {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("relay.db")).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for (subject, key) in [("17", "laptop"), ("17", "workstation"), ("99", "stranger")] {
            store
                .record_login(
                    "github",
                    identity(subject, "name"),
                    key.as_bytes(),
                    key.into(),
                    now,
                )
                .await
                .unwrap();
        }
        let octo = account(&store, "17").await;

        let mut cut = store
            .lapse(octo.id, now, None)
            .await
            .unwrap()
            .expect("a standing Account lapses");
        cut.sort();
        assert_eq!(cut, [b"laptop".to_vec(), b"workstation".to_vec()]);
        for key in ["laptop", "workstation"] {
            assert_eq!(store.standing(key.as_bytes(), None).await.unwrap(), None);
            assert_eq!(store.account_of(key.as_bytes(), None).await.unwrap(), None);
        }
        assert!(
            store
                .parties(b"laptop", b"workstation", octo.id, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.standing(b"stranger", None).await.unwrap().is_some());
        assert!(account(&store, "17").await.lapsed);
        assert_eq!(
            store.logins().await.unwrap().len(),
            3,
            "nothing is forgotten"
        );
        assert_eq!(
            store
                .standing_accounts()
                .await
                .unwrap()
                .iter()
                .map(|account| account.identity.subject.as_str())
                .collect::<Vec<_>>(),
            ["99"],
            "a lapsed Account is checked no more"
        );
        assert_eq!(store.lapse(octo.id, now, None).await.unwrap(), None);

        let restored = store
            .record_login(
                "github",
                identity("17", "octo"),
                b"laptop",
                "laptop".into(),
                now,
            )
            .await
            .unwrap();
        assert_eq!(restored.id, octo.id);
        assert!(!account(&store, "17").await.lapsed);
        assert!(
            store
                .standing(b"workstation", None)
                .await
                .unwrap()
                .is_some(),
            "one fresh login restores every Login under the Account"
        );
    }

    #[tokio::test]
    async fn a_login_stands_only_where_its_account_was_logged_in_as_since_a_fresh_login_is_required()
     {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("relay.db")).unwrap();
        let logged_in = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let second = Duration::from_secs(1);
        store
            .record_login(
                "github",
                identity("17", "octo"),
                b"laptop",
                "laptop".into(),
                logged_in,
            )
            .await
            .unwrap();
        assert!(
            store
                .standing(b"laptop", Some(logged_in - second))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.standing(b"laptop", Some(logged_in)).await.unwrap(),
            None
        );
        let read = store.standing_accounts().await.unwrap().remove(0);
        assert!(!read.is_due(Some(logged_in - second)) && read.is_due(Some(logged_in)));
        assert!(
            !read.is_due(None),
            "nothing is due where no fresh login is required"
        );

        // Logged in as afresh after it was found due, it is no longer due.
        store
            .record_login(
                "github",
                identity("17", "octo"),
                b"workstation",
                "workstation".into(),
                logged_in + second,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .lapse(read.id, logged_in + second, Some(read.logged_in_at))
                .await
                .unwrap(),
            None
        );
        assert!(!account(&store, "17").await.lapsed);
    }

    #[tokio::test]
    async fn records_written_before_accounts_could_lapse_are_carried_forward_standing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay.db");
        let mut connection = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        connection.run_next_migration(MIGRATIONS).unwrap();
        connection
            .batch_execute(
                "INSERT INTO accounts (id, created_at) VALUES (1, 100);
                 INSERT INTO identities (provider, subject, username, account_id)
                     VALUES ('github', '17', 'octo', 1);
                 INSERT INTO logins (server_key, fingerprint, account_id, hostname, formed_at)
                     VALUES (x'6b6579', 'fingerprint', 1, 'laptop', 1000);",
            )
            .unwrap();
        drop(connection);

        let store = Store::open(&path).unwrap();
        assert!(!account(&store, "17").await.lapsed);
        let at = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        assert!(store.standing(b"key", at(999)).await.unwrap().is_some());
        assert_eq!(
            store.standing(b"key", at(1000)).await.unwrap(),
            None,
            "the Account was last logged in as when its latest Login was formed"
        );
    }
}
