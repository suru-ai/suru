//! The Relay's records: its Accounts, the identities that log in as them,
//! and the Logins that stand under them, kept in SQLite.
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

diesel::table! {
    accounts (id) {
        id -> BigInt,
        created_at -> BigInt,
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
            .batch_execute(
                "PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;",
            )
            .context("configure the Relay's database")?;
        connection
            .run_pending_migrations(MIGRATIONS)
            .map_err(|error| anyhow::anyhow!("migrate the Relay's database: {error}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// The Account the Login tied to `server_key` stands under, by its
    /// provider and username, where there is such a Login.
    pub(crate) async fn standing(
        &self,
        server_key: &[u8],
    ) -> Result<Option<suru_relay_protocol::Account>> {
        let server_key = server_key.to_vec();
        self.run(move |connection| {
            logins::table
                .inner_join(identities::table.on(identities::account_id.eq(logins::account_id)))
                .filter(logins::server_key.eq(server_key))
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
    /// where there is such a Login.
    pub(crate) async fn account_of(&self, server_key: &[u8]) -> Result<Option<i64>> {
        let server_key = server_key.to_vec();
        self.run(move |connection| {
            logins::table
                .find(server_key)
                .select(logins::account_id)
                .first::<i64>(connection)
                .optional()
                .context("read a Server's Account")
        })
        .await
    }

    /// Records that `identity`, at `provider`, logged in for the Server whose
    /// key is `server_key`: finds the Account that identity answers to, or
    /// creates one, and ties a Login under it to the key, labelled with
    /// `hostname`. A Login the key already held is formed anew under it.
    pub(crate) async fn record_login(
        &self,
        provider: &str,
        identity: Identity,
        server_key: &[u8],
        hostname: String,
        now: SystemTime,
    ) -> Result<suru_relay_protocol::Account> {
        let provider = provider.to_owned();
        let server_key = server_key.to_vec();
        let now = unix_seconds(now);
        self.run(move |connection| {
            connection
                .transaction(|connection| {
                    let known = identities::table
                        .find((&provider, &identity.subject))
                        .select(identities::account_id)
                        .first::<i64>(connection)
                        .optional()?;
                    let account = match known {
                        Some(account) => account,
                        None => diesel::insert_into(accounts::table)
                            .values(accounts::created_at.eq(now))
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
                    diesel::QueryResult::Ok(())
                })
                .context("record a Login")?;
            Ok(suru_relay_protocol::Account {
                provider,
                username: identity.username,
            })
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
                ))
                .load::<(i64, String, String, String)>(connection)
                .map(|rows| {
                    rows.into_iter()
                        .map(|(id, provider, subject, username)| Account {
                            id,
                            provider,
                            subject,
                            username,
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
                .select((
                    logins::account_id,
                    logins::fingerprint,
                    logins::hostname,
                    logins::formed_at,
                ))
                .load::<(i64, String, String, i64)>(connection)
                .map(|rows| {
                    rows.into_iter()
                        .map(|(account, fingerprint, hostname, formed_at)| Login {
                            account,
                            fingerprint,
                            hostname,
                            formed_at: UNIX_EPOCH
                                + Duration::from_secs(u64::try_from(formed_at).unwrap_or(0)),
                        })
                        .collect()
                })
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
        assert_eq!(renamed.username, "octocat");
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
            store.standing(b"laptop").await.unwrap(),
            Some(suru_relay_protocol::Account {
                provider: "github".to_owned(),
                username: "octo".to_owned(),
            })
        );

        assert!(store.forget(b"laptop").await.unwrap());
        assert!(!store.forget(b"laptop").await.unwrap());
        assert_eq!(store.standing(b"laptop").await.unwrap(), None);
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
        assert!(reopened.standing(b"key").await.unwrap().is_some());
    }
}
