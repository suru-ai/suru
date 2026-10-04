//! The operator's command line: how a Relay's operator sees who is on it, and
//! takes someone, or one machine, off it — a departed user, a lost laptop. It
//! lists the Accounts and the Logins in the Relay's records and removes
//! either; beside its configuration, the Relay has no other administrative
//! interface.
//!
//! It runs as a process of its own, on the Relay's records alone, so the
//! Relay opens nothing to the network for it, and it works the same whether
//! or not the Relay is running on them. A removal is one step in the records:
//! it removes the Login — or the Account, with the identity that logs in as
//! it and every Login under it — and notes each Login removed for the Relay
//! to cut what stands on it. From the moment the step is taken, nothing is
//! made on the strength of a Login removed, since a running Relay reads a
//! Login from its records each time it decides anything on the strength of
//! one; and a running Relay looks for Logins removed every so often
//! ([`crate::RelayConfig::with_removal_interval`]), cutting at once, under
//! the standing lock, the connections their Servers hold there and the joins
//! it carries for them, each such join logged as any other is. A Login
//! formed again for the same key before then cuts them as it is formed, so
//! nothing that stood on the Login removed stands on the new one. A Server
//! whose Login is removed reads **login needed**, and logging in again forms
//! a new Login. Nothing the Relay does ends a Pairing: a Remote with a direct
//! way goes on working.
//!
//! Removing an Account keeps who and what the admission rules name. The
//! rules are the whole truth of who may use the Relay, so a user they still
//! admit may log in again, as a new Account: to keep someone out, the
//! operator takes them out of the rules.
//!
//! A list is a table for a reader, unless JSON is asked for, for a script.
//! Usernames come from the identity provider and hostnames from the Servers,
//! so a table shows each character in them that a terminal would act on, or
//! a reader not see, escaped; JSON carries them whole.

use std::{fmt::Write as _, io::Write, path::Path, time::Duration, time::SystemTime};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};

use crate::{
    connection::Relay,
    standing::Cut,
    store::{Account, Store},
};

/// The fewest characters of a fingerprint that name a Login to remove: as
/// many as a Server shows of one in short.
const SHORTEST_FINGERPRINT: usize = 8;

/// How the command line writes a time: RFC 3339, in UTC, to the second.
const TIME: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// What the operator asks of a Relay's records.
#[derive(Debug, Subcommand)]
pub enum OperatorCommand {
    /// Lists or removes the Accounts at this Relay: its users, as it knows
    /// them.
    #[command(subcommand)]
    Accounts(AccountsCommand),
    /// Lists or removes the Logins at this Relay: the Servers logged in at
    /// it, each under an Account.
    #[command(subcommand)]
    Logins(LoginsCommand),
}

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
    /// Lists every Account, in the order they were made.
    ///
    /// Each is named by the identity provider its user logs in through, and
    /// their ID there — numeric at GitHub, and never taken by anyone else, as
    /// a username may be — beside their username as of their latest login. It
    /// says how many Logins stand under the Account, and whether it has
    /// lapsed: its Logins refused, because the admission rules no longer admit
    /// its user, or because none of its Servers has logged in as recently as
    /// this Relay requires, until one of them logs in afresh. With --json, an
    /// Account's ID is its `subject`, as the connection log names it.
    List(Listing),
    /// Removes an Account, with every Login under it.
    ///
    /// The Account is named by its user's identity provider and their ID
    /// there, as `accounts list` shows them. Whatever the Logins under it
    /// hold at this Relay is cut at once, if it is running — the connections
    /// their Servers hold there, and every connection it carries for them —
    /// and their Servers must log in again to use it. No Pairing ends. The
    /// admission rules are kept as they are, so a user they still admit can
    /// log in again, as a new Account: to keep them out, take them out of the
    /// rules.
    Remove {
        /// The identity provider the Account's user logs in through, such as
        /// `github`.
        provider: String,
        /// The user's ID at that provider, as `accounts list` shows it.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum LoginsCommand {
    /// Lists every Login, in the order they were formed.
    ///
    /// Each is named by the fingerprint of its Server's identity key, beside
    /// the hostname the Server reported as it logged in, the Account it
    /// stands under — its user's identity provider, ID there, and username —
    /// and when it was formed.
    List(Listing),
    /// Removes a Login.
    ///
    /// The Login is named by its Server's key fingerprint, as `logins list`
    /// shows it, or by as much of the beginning of it as no other Login's
    /// shares, at least 8 characters. Whatever the Login holds at this Relay
    /// is cut at once, if it is running — the connections its Server holds
    /// there, and every connection it carries for it — and its Server must log
    /// in again to use it. No Pairing ends.
    Remove {
        /// The fingerprint of the Login's Server's identity key, or the
        /// beginning of it.
        fingerprint: String,
    },
}

/// How a list is printed.
#[derive(Debug, Args)]
pub struct Listing {
    /// Prints a JSON array, for a script, rather than a table.
    #[arg(long)]
    pub json: bool,
}

/// Carries out `command` on the Relay records at `database`, printing what it
/// found, or what it did, to `out`. Records that are not there are refused,
/// rather than made, as are records a newer Relay has carried forward; older
/// ones are carried forward, as the Relay itself would.
pub async fn operate(
    database: &Path,
    command: OperatorCommand,
    out: &mut impl Write,
) -> Result<()> {
    if !database.is_file() {
        bail!(
            "there is no Relay database at {}; name the one the Relay keeps its records in with \
             --database",
            database.display()
        );
    }
    let store = Store::open(database)?;
    let printed = match command {
        OperatorCommand::Accounts(AccountsCommand::List(Listing { json })) => {
            list_accounts(&store, json).await?
        }
        OperatorCommand::Accounts(AccountsCommand::Remove { provider, id }) => {
            remove_account(&store, &provider, &id).await?
        }
        OperatorCommand::Logins(LoginsCommand::List(Listing { json })) => {
            list_logins(&store, json).await?
        }
        OperatorCommand::Logins(LoginsCommand::Remove { fingerprint }) => {
            remove_login(&store, &fingerprint).await?
        }
    };
    out.write_all(printed.as_bytes())
        .and_then(|()| out.flush())
        .context("print what was asked")
}

/// An Account as a list in JSON names it.
#[derive(Serialize)]
struct AccountNamed<'account> {
    provider: &'account str,
    subject: &'account str,
    username: &'account str,
}

impl<'account> From<&'account Account> for AccountNamed<'account> {
    fn from(account: &'account Account) -> Self {
        Self {
            provider: &account.provider,
            subject: &account.subject,
            username: &account.username,
        }
    }
}

#[derive(Serialize)]
struct AccountListed<'account> {
    #[serde(flatten)]
    account: AccountNamed<'account>,
    lapsed: bool,
    logins: usize,
}

#[derive(Serialize)]
struct LoginListed<'login> {
    fingerprint: &'login str,
    hostname: &'login str,
    formed_at: String,
    account: AccountNamed<'login>,
}

async fn list_accounts(store: &Store, json: bool) -> Result<String> {
    let (accounts, logins) = store.listing().await?;
    let listed = accounts
        .iter()
        .map(|account| AccountListed {
            account: account.into(),
            lapsed: account.lapsed,
            logins: logins
                .iter()
                .filter(|login| login.account == account.id)
                .count(),
        })
        .collect::<Vec<_>>();
    if json {
        return as_json(&listed);
    }
    Ok(table(
        &["PROVIDER", "ID", "USERNAME", "LOGINS", "LAPSED"],
        listed.iter().map(|listed| {
            vec![
                shown(listed.account.provider),
                shown(listed.account.subject),
                shown(listed.account.username),
                listed.logins.to_string(),
                if listed.lapsed { "yes" } else { "no" }.to_owned(),
            ]
        }),
    ))
}

async fn list_logins(store: &Store, json: bool) -> Result<String> {
    let (accounts, logins) = store.listing().await?;
    let listed = logins
        .iter()
        .filter_map(|login| {
            let account = accounts
                .iter()
                .find(|account| account.id == login.account)?;
            Some(LoginListed {
                fingerprint: &login.fingerprint,
                hostname: &login.hostname,
                formed_at: time(login.formed_at),
                account: account.into(),
            })
        })
        .collect::<Vec<_>>();
    if json {
        return as_json(&listed);
    }
    Ok(table(
        &[
            "FINGERPRINT",
            "HOSTNAME",
            "PROVIDER",
            "ID",
            "USERNAME",
            "FORMED",
        ],
        listed.iter().map(|listed| {
            vec![
                shown(listed.fingerprint),
                shown(listed.hostname),
                shown(listed.account.provider),
                shown(listed.account.subject),
                shown(listed.account.username),
                listed.formed_at.clone(),
            ]
        }),
    ))
}

async fn remove_login(store: &Store, fingerprint: &str) -> Result<String> {
    let prefix = fingerprint.trim().to_ascii_lowercase();
    if !prefix
        .chars()
        .all(|character| character.is_ascii_hexdigit())
    {
        bail!(
            "`{}` is not a key fingerprint, which is written in hexadecimal, as `suru-relay logins \
             list` shows it",
            shown(fingerprint)
        );
    }
    if prefix.len() < SHORTEST_FINGERPRINT {
        bail!(
            "name a Login by at least {SHORTEST_FINGERPRINT} characters of its key fingerprint, as \
             `suru-relay logins list` shows it, not {}",
            prefix.len()
        );
    }
    let found = store.remove_login(&prefix, SystemTime::now()).await?;
    match found.as_slice() {
        [] => bail!(
            "this Relay has no Login whose key fingerprint begins {prefix}; `suru-relay logins \
             list` lists them"
        ),
        [(login, account)] => Ok(format!(
            "Removed the Login of {}, {}, under the Account {}: its Server must log in again to \
             use this Relay. No Pairing ends.\n",
            shown(&login.hostname),
            login.fingerprint,
            account_named(account)
        )),
        several => {
            let mut said = format!(
                "{} Logins have a key fingerprint beginning {prefix}, so none was removed; name \
                 one by more of its fingerprint:",
                several.len()
            );
            for (login, account) in several {
                let _ = write!(
                    said,
                    "\n  {}  {}, under the Account {}",
                    login.fingerprint,
                    shown(&login.hostname),
                    account_named(account)
                );
            }
            bail!(said)
        }
    }
}

async fn remove_account(store: &Store, provider: &str, id: &str) -> Result<String> {
    let Some((account, logins)) = store
        .remove_account(provider, id, SystemTime::now())
        .await?
    else {
        bail!(
            "this Relay has no Account of the user {} at {}; `suru-relay accounts list` lists \
             them",
            shown(id),
            shown(provider)
        );
    };
    let named = account_named(&account);
    let logins = match logins.len() {
        0 => "no Login stood under it".to_owned(),
        1 => "the Login under it: its Server must log in again to use this Relay".to_owned(),
        count => format!(
            "the {count} Logins under it: their Servers must log in again to use this Relay"
        ),
    };
    Ok(format!(
        "Removed the Account {named} and {logins}. No Pairing ends.\n\
         While this Relay's admission rules admit {named}, they can log in again, as a new \
         Account; to keep them out, take them out of the rules.\n"
    ))
}

/// An Account as the command line names it: its user's username, their
/// identity provider, and their ID there.
fn account_named(account: &Account) -> String {
    format!(
        "{} ({} {})",
        shown(&account.username),
        shown(&account.provider),
        shown(&account.subject)
    )
}

fn as_json(listed: &impl Serialize) -> Result<String> {
    let mut json = serde_json::to_string(listed).context("write the list as JSON")?;
    json.push('\n');
    Ok(json)
}

/// `rows` beneath `headings`, a column two spaces from the next, each as wide
/// as its widest cell but the last, which runs to the end of its line.
fn table(headings: &[&str], rows: impl Iterator<Item = Vec<String>>) -> String {
    let rows = std::iter::once(headings.iter().map(|&heading| heading.to_owned()).collect())
        .chain(rows)
        .collect::<Vec<Vec<String>>>();
    let widths = (0..headings.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut table = String::new();
    for row in &rows {
        for (column, cell) in row.iter().enumerate() {
            if column + 1 == row.len() {
                table.push_str(cell);
            } else {
                let _ = write!(table, "{cell:<width$}  ", width = widths[column]);
            }
        }
        table.push('\n');
    }
    table
}

/// `text` as a table shows it: each character a terminal would act on, or a
/// reader not see, escaped as `\u{1b}`, and each backslash doubled, so that
/// nothing escaped reads as what was written.
fn shown(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\\' {
            shown.push_str("\\\\");
        } else if unseen(character) {
            let _ = write!(shown, "\\u{{{:x}}}", u32::from(character));
        } else {
            shown.push(character);
        }
    }
    shown
}

/// Whether a terminal acts on `character`, or a reader cannot see it: a
/// control character, a line or paragraph separator, a formatting character
/// — such as one reversing the direction of what follows — or one a reader's
/// display ignores.
fn unseen(character: char) -> bool {
    use icu_properties::{
        CodePointMapData, CodePointSetData,
        props::{DefaultIgnorableCodePoint, GeneralCategory},
    };
    character.is_control()
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
        || matches!(
            CodePointMapData::<GeneralCategory>::new().get(character),
            GeneralCategory::Format
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
        )
}

fn time(time: SystemTime) -> String {
    OffsetDateTime::from(time).format(TIME).unwrap_or_default()
}

/// Cuts everything standing on the Logins the operator has removed since
/// the Relay last looked, under the standing lock: the joins asked between
/// them, the joins carried, and the connections their Servers hold on the
/// strength of them, each told its Server must log in again.
pub(crate) async fn cut_removed(relay: &Relay) -> Result<()> {
    let standing = relay.standing.lock().await;
    let removed = relay.store.take_removed().await?;
    if removed.is_empty() {
        return Ok(());
    }
    relay.cut(&standing, &removed, Cut::Refused);
    drop(standing);
    for key in &removed {
        tracing::info!(
            fingerprint = suru_relay_protocol::fingerprint(key),
            "a Login the operator removed was cut"
        );
    }
    Ok(())
}

/// Cuts what stands on the Logins the operator removes, looking for them
/// every `interval`, until what awaits this is dropped.
pub(crate) async fn keep_cutting_removed(relay: &Relay, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        if let Err(error) = cut_removed(relay).await {
            tracing::error!(
                "the Relay could not look for Logins its operator removed, as its records could \
                 not be used: {error:#}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_lines_its_columns_up_and_runs_its_last_to_the_end_of_each_line() {
        assert_eq!(
            table(
                &["A", "LONGER", "LAST"],
                [
                    vec!["wide cell".to_owned(), "x".to_owned(), "1".to_owned()],
                    vec!["y".to_owned(), "z".to_owned(), "a longer last".to_owned()],
                ]
                .into_iter()
            ),
            "A          LONGER  LAST\n\
             wide cell  x       1\n\
             y          z       a longer last\n"
        );
    }

    #[test]
    fn what_a_terminal_would_act_on_or_a_reader_not_see_is_shown_escaped() {
        assert_eq!(shown("laptop"), "laptop");
        assert_eq!(shown("Jake's Mac — 東京"), "Jake's Mac — 東京");
        assert_eq!(shown("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(shown("tab\there\nnext"), "tab\\u{9}here\\u{a}next");
        assert_eq!(shown("exe\u{202e}gpj"), "exe\\u{202e}gpj");
        assert_eq!(
            shown("zero\u{200b}width\u{2028}"),
            "zero\\u{200b}width\\u{2028}"
        );
        assert_eq!(shown("back\\u{1b}slash"), "back\\\\u{1b}slash");
    }
}
