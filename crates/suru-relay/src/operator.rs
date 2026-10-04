//! The operator's command line: how a Relay's operator sees who is on it, and
//! takes someone, or one machine, off it — a departed user, a lost laptop. It
//! lists the Accounts and the Logins in the Relay's records and removes
//! either; beside its configuration, the Relay has no other administrative
//! interface.
//!
//! It runs as a process of its own, on the Relay's records alone, so the
//! Relay opens nothing to the network for it, and it works the same whether
//! or not the Relay is running on them. It reads them only as the Relay that
//! runs on them last carried them forward, never carrying them forward
//! itself, which only a Relay that runs does.
//!
//! A removal is one step in the records: it removes the Login — or the
//! Account, with the identity that logs in as it and every Login under it —
//! and notes each Login removed, by a number never given again, for the
//! Relay to cut what stands on it. From the moment the step is taken, nothing
//! is made on the strength of a Login removed, since a running Relay reads a
//! Login from its records each time it decides anything on the strength of
//! one; whatever it had already begun on the strength of one, having read it
//! just before, may finish, and the cut that follows closes it. A running
//! Relay looks for removals every so often
//! ([`crate::RelayConfig::with_removal_interval`]) and cuts at once, under
//! the standing lock, a batch at a time, the connections the removed Logins'
//! Servers hold there and the joins it carries for them, each such join
//! logged as any other is. Once what it cut has let go it confirms the cut by
//! forgetting the removals, and where it cannot it cuts them again — which
//! cuts nothing more — and confirms them the next time it looks. The removal
//! waits for that confirmation, for as long as it is told, where a Relay is
//! running on the records — as the lock a running Relay holds beside them
//! says — and returns at once where none is, there being nothing to cut.
//!
//! A Login formed again for the same key before the Relay looks cuts, as it
//! is formed, what stood on the one removed, and confirms the removal, so
//! nothing that stood on the Login removed stands on the new one, and the
//! Relay's next look finds nothing of it to cut. A Server whose Login is
//! removed reads **login needed**, and logging in again forms a new Login.
//! Nothing the Relay does ends a Pairing: a Remote with a direct way goes on
//! working.
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

use std::{
    fmt::Write as _,
    io::{ErrorKind, Write},
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};

use crate::{
    connection::Relay,
    running,
    standing::Cut,
    store::{Account, Store},
};

/// The fewest characters of a fingerprint that name a Login to remove: as
/// many as a Server shows of one in short.
const SHORTEST_FINGERPRINT: usize = 8;

/// How many removals a Relay cuts at once, under the standing lock, before
/// confirming them and going on to the next.
pub const REMOVALS_CUT_AT_ONCE: usize = 512;

/// How long a Relay waits for what it cut to let go — each connection
/// closed, each join's line handed to the connection log — before it
/// confirms the cut all the same: a Server that will not take in that its
/// connection is closing holds the confirmation up no longer.
const LETTING_GO: Duration = Duration::from_secs(1);

/// How often a removal looks to see whether the Relay running on the records
/// has confirmed it.
const CONFIRMATION_POLL: Duration = Duration::from_millis(20);

/// How long a removal waits for the Relay running on the records to confirm
/// it, unless told otherwise.
const CONFIRMATION_WAIT: Duration = Duration::from_secs(10);

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
    /// there, as `accounts list` shows them. Its Logins are refused from the
    /// moment they are removed, and whatever they hold at this Relay, if it
    /// is running, is cut at once — the connections their Servers hold there,
    /// and every connection it carries for them — the command returning once
    /// the Relay has confirmed it. Anything the Relay had already begun on the
    /// strength of one as it was removed may finish, and the cut closes it.
    /// Their Servers must log in again to use the Relay. No Pairing ends. The
    /// admission rules are kept as they are, so a user they still admit can
    /// log in again, as a new Account: to keep them out, take them out of the
    /// rules.
    Remove {
        /// The identity provider the Account's user logs in through, such as
        /// `github`.
        provider: String,
        /// The user's ID at that provider, as `accounts list` shows it.
        id: String,
        #[command(flatten)]
        confirmation: Confirmation,
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
    /// shares, at least 8 characters. It is refused from the moment it is
    /// removed, and whatever it holds at this Relay, if it is running, is cut
    /// at once — the connections its Server holds there, and every connection
    /// it carries for it — the command returning once the Relay has confirmed
    /// it. Anything the Relay had already begun on the strength of the Login
    /// as it was removed — taking up a join, say — may finish, and the cut
    /// closes it. Its Server must log in again to use the Relay. No Pairing
    /// ends.
    Remove {
        /// The fingerprint of the Login's Server's identity key, or the
        /// beginning of it.
        fingerprint: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
}

/// How a list is printed.
#[derive(Debug, Args)]
pub struct Listing {
    /// Prints a JSON array, for a script, rather than a table.
    #[arg(long)]
    pub json: bool,
}

/// How long a removal waits for the Relay to confirm it.
#[derive(Debug, Args)]
pub struct Confirmation {
    /// How long, in seconds, to wait for a Relay running on the records to
    /// confirm it has cut every connection the removed Logins' Servers held
    /// there. Past it, the command says the removal stands, refused already,
    /// but that the Relay has not confirmed the cut, and exits with status 3.
    #[arg(
        long = "wait",
        value_name = "SECONDS",
        default_value = "10",
        value_parser = seconds
    )]
    pub wait: Duration,
}

impl Default for Confirmation {
    fn default() -> Self {
        Self {
            wait: CONFIRMATION_WAIT,
        }
    }
}

fn seconds(text: &str) -> Result<Duration, String> {
    text.parse::<f64>()
        .ok()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| format!("`{text}` is not a number of seconds"))
}

/// How a command the operator ran came out, where it did not fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// It did all it was asked.
    Done,
    /// It made the removal it was asked to, which the Relay refuses from
    /// then on, but the Relay running on the records did not confirm in time
    /// that it had cut what stood on what was removed.
    CutUnconfirmed,
}

/// Carries out `command` on the Relay records at `database`, printing what it
/// found, or what it did, to `out`, and on `err` what a removal whose cut the
/// running Relay did not confirm in time comes to. Records that are not there
/// are refused, rather than made, as are records a newer Relay has carried
/// forward and those an older one left, which only a Relay that runs carries
/// forward. A reader of `out` that has gone takes nothing from what was done.
pub async fn operate(
    database: &Path,
    command: OperatorCommand,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Outcome> {
    if !database.is_file() {
        bail!(
            "there is no Relay database at {}; name the one the Relay keeps its records in with \
             --database",
            database.display()
        );
    }
    let store = Store::open_as_they_are(database)?;
    let (printed, unconfirmed) = match command {
        OperatorCommand::Accounts(AccountsCommand::List(Listing { json })) => {
            (list_accounts(&store, json).await?, None)
        }
        OperatorCommand::Accounts(AccountsCommand::Remove {
            provider,
            id,
            confirmation,
        }) => remove_account(&store, database, &provider, &id, confirmation.wait).await?,
        OperatorCommand::Logins(LoginsCommand::List(Listing { json })) => {
            (list_logins(&store, json).await?, None)
        }
        OperatorCommand::Logins(LoginsCommand::Remove {
            fingerprint,
            confirmation,
        }) => remove_login(&store, database, &fingerprint, confirmation.wait).await?,
    };
    print(out, &printed)?;
    match unconfirmed {
        Some(said) => {
            print(err, &said)?;
            Ok(Outcome::CutUnconfirmed)
        }
        None => Ok(Outcome::Done),
    }
}

/// Writes `text` to `to` whole, unless its reader has gone.
fn print(to: &mut impl Write, text: &str) -> Result<()> {
    match to.write_all(text.as_bytes()).and_then(|()| to.flush()) {
        Err(error) if error.kind() != ErrorKind::BrokenPipe => {
            Err(error).context("print what was asked")
        }
        _ => Ok(()),
    }
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

/// Removes the Login `fingerprint` names: what to print of it, and what to
/// say where the Relay running on the records did not confirm the cut in
/// time.
async fn remove_login(
    store: &Store,
    database: &Path,
    fingerprint: &str,
    wait: Duration,
) -> Result<(String, Option<String>)> {
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
    let (found, noted) = store.remove_login(&prefix, SystemTime::now()).await?;
    match found.as_slice() {
        [] => bail!(
            "this Relay has no Login whose key fingerprint begins {prefix}; `suru-relay logins \
             list` lists them"
        ),
        [(login, account)] => {
            let removed = format!(
                "Removed the Login of {}, {}, under the Account {}: its Server must log in again \
                 to use this Relay. No Pairing ends.\n",
                shown(&login.hostname),
                login.fingerprint,
                account_named(account)
            );
            Ok(
                match confirmed(store, database, noted, wait, "its Server").await {
                    Ok(cut) => (removed + &cut, None),
                    Err(unconfirmed) => (removed, Some(unconfirmed)),
                },
            )
        }
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

/// Removes the Account of the user `id` at `provider`: what to print of it,
/// and what to say where the Relay running on the records did not confirm
/// the cut in time.
async fn remove_account(
    store: &Store,
    database: &Path,
    provider: &str,
    id: &str,
    wait: Duration,
) -> Result<(String, Option<String>)> {
    let Some((account, logins, noted)) = store
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
    let (under_it, held) = match logins.len() {
        0 => ("no Login stood under it".to_owned(), None),
        1 => (
            "the Login under it: its Server must log in again to use this Relay".to_owned(),
            Some("its Server"),
        ),
        count => (
            format!(
                "the {count} Logins under it: their Servers must log in again to use this Relay"
            ),
            Some("their Servers"),
        ),
    };
    let mut removed = format!("Removed the Account {named} and {under_it}. No Pairing ends.\n");
    let mut unconfirmed = None;
    if let Some(held) = held {
        match confirmed(store, database, noted, wait, held).await {
            Ok(cut) => removed.push_str(&cut),
            Err(said) => unconfirmed = Some(said),
        }
    }
    removed.push_str(&format!(
        "While this Relay's admission rules admit {named}, they can log in again, as a new \
         Account; to keep them out, take them out of the rules.\n"
    ));
    Ok((removed, unconfirmed))
}

/// Waits, no longer than `wait`, for a Relay running on the records at
/// `database` to confirm it has cut what stood on the removals numbered
/// `noted`, made of Logins `held` — the Servers they were — held: what to
/// print of the cut where the Relay confirmed it, or there is none running to
/// cut anything, or else what to say of a cut it did not confirm.
async fn confirmed(
    store: &Store,
    database: &Path,
    noted: Vec<i64>,
    wait: Duration,
    held: &str,
) -> Result<String, String> {
    if !running::may_be_running(database) {
        return Ok(format!(
            "No Relay is running on these records, so {held} held no connection there to cut.\n"
        ));
    }
    let deadline = tokio::time::Instant::now() + wait;
    let why = loop {
        match store.any_still_removed(noted.clone()).await {
            Ok(false) => {
                return Ok(format!(
                    "The Relay running on these records has cut every connection {held} held \
                     there.\n"
                ));
            }
            Ok(true) => {}
            Err(error) => break format!(", as its records could not be read: {error:#}"),
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break String::new();
        }
        tokio::time::sleep(CONFIRMATION_POLL.min(deadline - now)).await;
    };
    Err(format!(
        "The removal stands, and is refused from now on, but the Relay running on these records \
         did not confirm within {wait:?} that it had cut every connection {held} held there{why}. \
         It cuts them once it next looks for removals, and they end if it stops.\n"
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

/// Cuts everything standing on the Logins the operator has removed, and the
/// Relay has yet to confirm cutting: the joins asked between them, the joins
/// carried, and the connections their Servers hold on the strength of them,
/// each told its Server must log in again. It cuts them a batch at a time,
/// in the order they were removed, each under the standing lock, and once
/// what it cut has let go — or no sooner than [`LETTING_GO`] has passed —
/// confirms the batch by forgetting its removals. A batch it cannot confirm
/// is cut, and confirmed, again the next time the Relay looks; it goes on to
/// the batches after it all the same, so no removal waits on another.
pub(crate) async fn cut_removed(relay: &Relay) -> Result<()> {
    // Most looks find nothing, and take nothing a Server's connection waits
    // on.
    if !relay.store.any_removed().await? {
        return Ok(());
    }
    let mut after = 0;
    loop {
        let (removed, released) = {
            let standing = relay.standing.lock().await;
            let removed = relay
                .store
                .removed_after(after, REMOVALS_CUT_AT_ONCE)
                .await?;
            let keys = removed
                .iter()
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>();
            let released = relay.cut(&standing, &keys, Cut::Refused);
            (removed, released)
        };
        let Some(&(last, _)) = removed.last() else {
            return Ok(());
        };
        let _ = tokio::time::timeout(LETTING_GO, released.all()).await;
        let numbers = removed.iter().map(|(number, _)| *number).collect();
        match relay.store.forget_removed(numbers).await {
            Ok(()) => {
                for (_, key) in &removed {
                    tracing::info!(
                        fingerprint = suru_relay_protocol::fingerprint(key),
                        "a Login the operator removed was cut"
                    );
                }
            }
            Err(error) => tracing::warn!(
                "the Relay cut what stood on {} Logins the operator removed, and could not \
                 confirm it in its records, so it will cut them and confirm them again: {error:#}",
                removed.len()
            ),
        }
        if removed.len() < REMOVALS_CUT_AT_ONCE {
            return Ok(());
        }
        after = last;
    }
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
