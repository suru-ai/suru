//! A Server's Relays on its own API: the entries its user lists, adds and
//! removes, and the logins the Server carries out at them (ADR-0045,
//! ADR-0048). This is server administration, refused to Peers and offered to
//! no Sidekick.

use serde::{Deserialize, Serialize};

/// The event a followed login reports its progress under.
pub const RELAY_LOGIN_EVENT: &str = "relay_login";

/// A Relay the Server holds an entry for, known by its address.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Relay {
    pub address: String,
    pub state: RelayState,
    /// Why the Relay reads Unreachable, where it does.
    pub unreachable: Option<RelayUnreachable>,
    /// The Account the Server's Login there stands under, as the Relay last
    /// named it.
    pub account: Option<RelayAccount>,
    /// The latest login the Server began there since it started, where it
    /// began one.
    pub login: Option<RelayLogin>,
}

/// How a Relay stands for the Server.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayState {
    /// The Server holds a Login there that stands.
    LoggedIn,
    /// The Server holds no Login there that stands — it has not logged in
    /// there, or the Relay refuses the Login it holds — so trying cannot help
    /// until its user logs in.
    LoginNeeded,
    /// The Server cannot speak to the Relay while its Login stands — the
    /// Relay has stopped answering, or the two share no version of the
    /// protocol between them — and the Server keeps trying on its own.
    Unreachable,
}

/// Why a Relay reads Unreachable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayUnreachable {
    /// Where the Server and the Relay share no version of the protocol
    /// between them, the side to upgrade.
    pub behind: Option<RelaySide>,
    /// Says why to a reader.
    pub message: String,
}

/// One of the two sides of a Server's connection to a Relay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelaySide {
    Server,
    Relay,
}

/// The Account a Login stands under, by its identity provider and the
/// username it has there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccount {
    pub provider: String,
    pub username: String,
}

/// A login the Server carries out at a Relay: its user visits
/// `verification_uri` on any device and enters `user_code` there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayLogin {
    pub verification_uri: String,
    pub user_code: String,
    pub outcome: RelayLoginOutcome,
}

/// Where a login stands.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayLoginOutcome {
    /// The user has yet to log in.
    Pending,
    /// The login is done, and the Server's Login stands under `account`.
    Done { account: RelayAccount },
    /// The login ended with no Login formed; `message` says why to a reader.
    Refused {
        reason: RelayLoginRefusal,
        message: String,
    },
}

impl RelayLoginOutcome {
    /// Whether the login has ended, one way or the other.
    pub fn is_settled(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// Why a login formed no Login.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayLoginRefusal {
    /// The user refused it at the identity provider.
    Denied,
    /// Nobody finished it before it expired.
    Expired,
    /// The Relay could not log anyone in.
    Unavailable,
    /// The Relay stopped answering before it ended.
    Interrupted,
    /// The Relay logged the Server in, but the Server could not record its
    /// Login; logging in again records it.
    Unrecorded,
}

/// A Relay to add, by the address it is reached at.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AddRelayRequest {
    pub address: String,
}

/// The outcome of removing a Relay. Removal always forgets the entry here;
/// `acknowledged` says whether the Relay answered in time and forgot the
/// Server's Login as well.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayRemoval {
    pub address: String,
    pub acknowledged: bool,
}
