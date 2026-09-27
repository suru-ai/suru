//! Who may reach the Broker: the endpoint and per-Session token a Provider
//! start is handed, the registry of tokens still live, and the resolution of a
//! presented token to the Session it names.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use uuid::Uuid;

use crate::protocol::{SessionId, SettingsSnapshot};

/// The Broker endpoint's URL, on the Server's loopback listener.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerEndpoint(String);

impl BrokerEndpoint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BrokerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The bearer token naming one Session's Provider connection to the Broker.
/// It is a secret any process holding it could act under, so its `Debug` form
/// never prints it and it has no `Display` form to be logged by accident.
#[derive(Clone, Eq, PartialEq)]
pub struct BrokerToken(String);

impl BrokerToken {
    /// A fresh token: 244 random bits from two version-4 UUIDs, spelled as
    /// hexadecimal so it is a valid header value as it stands.
    fn mint() -> Self {
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    /// The token itself, for the harness that must present it.
    pub fn secret(&self) -> &str {
        &self.0
    }

    /// The `Authorization` header value presenting this token.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for BrokerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrokerToken(<redacted>)")
    }
}

/// What a Provider start is handed so its Agent can reach the Broker: the
/// endpoint, and the token naming the Session it runs for. Each start of a
/// Session's Provider is handed a token of its own, which is retired when that
/// Provider connection closes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerHandoff {
    endpoint: BrokerEndpoint,
    token: BrokerToken,
}

impl BrokerHandoff {
    pub fn endpoint(&self) -> &BrokerEndpoint {
        &self.endpoint
    }

    pub fn token(&self) -> &BrokerToken {
        &self.token
    }

    /// The HTTP header presenting the token, as the name and value a harness
    /// sends on every request it makes to the endpoint.
    pub fn authorization_header(&self) -> (&'static str, String) {
        ("Authorization", self.token.bearer())
    }
}

#[cfg(test)]
impl BrokerHandoff {
    /// A handoff naming `endpoint` with a fresh token no Broker holds, for a
    /// test of how a harness lowers one onto its own seam.
    pub(crate) fn for_tests(endpoint: &str) -> Self {
        Self {
            endpoint: BrokerEndpoint(endpoint.to_owned()),
            token: BrokerToken::mint(),
        }
    }
}

/// The Session a Broker request came from, resolved from the token it
/// presented before any Tool runs. This is the one answer a Tool has to "which
/// Session is calling", so none re-reads a header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrokerCaller {
    session_id: SessionId,
}

impl BrokerCaller {
    pub(crate) fn session_id(self) -> SessionId {
        self.session_id
    }
}

/// Grants Provider Sessions access to the Broker, and answers which Session a
/// presented token names. Cloned into whatever starts Provider connections and
/// into the endpoint; every clone reads and writes one registry.
#[derive(Clone)]
pub(crate) struct BrokerAccess {
    endpoint: BrokerEndpoint,
    /// Read at every grant and every request rather than once, so turning the
    /// Broker off reaches the next Provider start and the very next request.
    settings: watch::Receiver<SettingsSnapshot>,
    live: Arc<Mutex<LiveTokens>>,
}

#[derive(Default)]
struct LiveTokens {
    next_id: u64,
    tokens: HashMap<u64, LiveToken>,
}

struct LiveToken {
    token: BrokerToken,
    caller: BrokerCaller,
}

impl BrokerAccess {
    pub(crate) fn new(endpoint: String, settings: watch::Receiver<SettingsSnapshot>) -> Self {
        Self {
            endpoint: BrokerEndpoint(endpoint),
            settings,
            live: Arc::default(),
        }
    }

    /// Whether the user has left the Broker on.
    pub(crate) fn is_enabled(&self) -> bool {
        self.settings.borrow().settings.broker.enabled
    }

    /// Mints a token naming `session_id` for one Provider connection, live
    /// until the returned grant is dropped. Nothing while the Broker is off,
    /// so a Provider started then is handed no endpoint at all.
    pub(crate) fn grant(&self, session_id: SessionId) -> Option<BrokerGrant> {
        if !self.is_enabled() {
            return None;
        }
        let token = BrokerToken::mint();
        let id = {
            let mut live = self.live.lock().expect("Broker token lock is not poisoned");
            let id = live.next_id;
            live.next_id += 1;
            live.tokens.insert(
                id,
                LiveToken {
                    token: token.clone(),
                    caller: BrokerCaller { session_id },
                },
            );
            id
        };
        Some(BrokerGrant {
            live: self.live.clone(),
            id,
            handoff: BrokerHandoff {
                endpoint: self.endpoint.clone(),
                token,
            },
        })
    }

    /// The Session whose live token `headers` present as a bearer credential,
    /// or nothing for a token never minted, one already retired, or none at
    /// all. Every live token is compared in constant time, so how long the
    /// answer takes says nothing about how close a guess came.
    pub(crate) fn caller(&self, headers: &HeaderMap) -> Option<BrokerCaller> {
        let presented = headers.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, token) = presented.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let token = token.trim().as_bytes();
        self.live
            .lock()
            .expect("Broker token lock is not poisoned")
            .tokens
            .values()
            .find(|live| bool::from(live.token.0.as_bytes().ct_eq(token)))
            .map(|live| live.caller)
    }
}

/// One live token and the handoff carrying it, held beside the Provider
/// connection it was minted for. Dropping it retires the token, so a
/// connection that closes by any path — lost, replaced, or shut down — can
/// never leave its token answering.
pub(crate) struct BrokerGrant {
    live: Arc<Mutex<LiveTokens>>,
    id: u64,
    handoff: BrokerHandoff,
}

impl BrokerGrant {
    pub(crate) fn handoff(&self) -> &BrokerHandoff {
        &self.handoff
    }
}

impl Drop for BrokerGrant {
    fn drop(&mut self) {
        // Never panic in a destructor: a poisoned lock still holds the map.
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.tokens.remove(&self.id);
    }
}

impl fmt::Debug for BrokerGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerGrant")
            .field("handoff", &self.handoff)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;
    use crate::protocol::{BrokerSettings, EffectiveSettings};

    fn settings(enabled: bool) -> SettingsSnapshot {
        SettingsSnapshot {
            settings: EffectiveSettings {
                broker: BrokerSettings { enabled },
                ..EffectiveSettings::default()
            },
            ..SettingsSnapshot::default()
        }
    }

    fn access(enabled: bool) -> (BrokerAccess, watch::Sender<SettingsSnapshot>) {
        let (sender, receiver) = watch::channel(settings(enabled));
        (
            BrokerAccess::new("http://127.0.0.1:1/broker".to_owned(), receiver),
            sender,
        )
    }

    fn presenting(authorization: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(authorization).expect("a valid header value"),
        );
        headers
    }

    #[test]
    fn a_granted_token_names_its_session_until_the_grant_is_dropped() {
        let (access, _settings) = access(true);
        let session_id = SessionId::new();
        let grant = access.grant(session_id).expect("the Broker is on");
        let headers = presenting(&grant.handoff().token().bearer());

        assert_eq!(
            access.caller(&headers).map(BrokerCaller::session_id),
            Some(session_id)
        );
        assert_eq!(
            access.caller(&presenting(&format!(
                "bearer {}",
                grant.handoff().token().secret()
            ))),
            Some(BrokerCaller { session_id }),
            "the authentication scheme is matched regardless of case"
        );
        drop(grant);
        assert_eq!(
            access.caller(&headers),
            None,
            "a retired token names no one"
        );
    }

    #[test]
    fn every_grant_mints_a_token_of_its_own() {
        let (access, _settings) = access(true);
        let session_id = SessionId::new();
        let first = access.grant(session_id).expect("the Broker is on");
        let second = access.grant(session_id).expect("the Broker is on");
        assert_ne!(first.handoff().token(), second.handoff().token());
        assert_eq!(first.handoff().endpoint(), second.handoff().endpoint());

        drop(first);
        assert_eq!(
            access.caller(&presenting(&second.handoff().token().bearer())),
            Some(BrokerCaller { session_id }),
            "retiring one connection's token leaves the next one's live"
        );
    }

    #[test]
    fn nothing_is_granted_while_the_broker_is_off() {
        let (access, settings_sender) = access(false);
        assert!(access.grant(SessionId::new()).is_none());
        settings_sender.send_replace(settings(true));
        assert!(
            access.grant(SessionId::new()).is_some(),
            "turning the Broker back on reaches the very next grant"
        );
    }

    #[test]
    fn an_unknown_or_malformed_credential_names_no_one() {
        let (access, _settings) = access(true);
        let grant = access.grant(SessionId::new()).expect("the Broker is on");
        for refused in [
            "Bearer made-up".to_owned(),
            grant.handoff().token().secret().to_owned(),
            format!("Basic {}", grant.handoff().token().secret()),
            "Bearer".to_owned(),
        ] {
            assert_eq!(access.caller(&presenting(&refused)), None, "{refused:?}");
        }
        assert_eq!(access.caller(&HeaderMap::new()), None);
    }

    #[test]
    fn a_handoff_never_prints_its_token() {
        let (access, _settings) = access(true);
        let grant = access.grant(SessionId::new()).expect("the Broker is on");
        let printed = format!("{grant:?}");
        assert!(!printed.contains(grant.handoff().token().secret()));
        assert!(printed.contains("http://127.0.0.1:1/broker"));
    }
}
