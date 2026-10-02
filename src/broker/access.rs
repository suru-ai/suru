//! Who may reach the Broker: the endpoint and per-Session token a Provider
//! start is handed, the registry of tokens still live, and the resolution of a
//! presented token to the Session it names and what that Session's Agent is to
//! the Broker — a Sidekick, or any other Agent (ADR 0042).
//!
//! A Sidekick's handoff also carries what it begins knowing of Memories: the
//! titles of those most recently changed, read as its token is minted. A
//! token is minted at every start of a Session's Provider — its first, and
//! each relaunch, after a Server stop or a lost connection among them — so a
//! Sidekick is told of Memories as they stood when its Provider was last
//! started, and one whose Provider runs on is not told of a Memory stored or
//! changed since, which it finds by searching.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    memories::{MemoryIndex, MemoryStore},
    protocol::{Session, SessionId, SettingsSnapshot},
    provider::ProviderSubagentId,
    sessions::SessionStore,
    sidekick::SidekickWorkspace,
};

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

/// What a Broker caller is to the Broker, which decides the Tools it is
/// offered and what its Agent is told of them (ADR 0042). It is fixed when a
/// Provider start's token is minted, from the Session's stored Workspace and
/// its place in the tree, so nothing about a Session says so and no Provider
/// learns of it: every harness lowers the one `suru` server and the note it is
/// handed alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokerRole {
    /// Any Agent but a Sidekick — a Sidekick's own Subagents among them —
    /// offered the Tools through which it reaches any Provider Suru hosts.
    Agent,
    /// The Agent of a top-level Session in the Sidekick Workspace, offered
    /// besides those the Tools that work across Suru itself.
    Sidekick,
}

impl BrokerRole {
    /// What `session`'s Agent is to the Broker on the Server whose Sidekick
    /// Workspace is `sidekick`.
    pub(crate) fn of(session: &Session, sidekick: &SidekickWorkspace) -> Self {
        if sidekick.is_sidekicks(session) {
            Self::Sidekick
        } else {
            Self::Agent
        }
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
    /// What the Session's Agent is to the Broker, which the note it is told
    /// is written for and which no harness reads otherwise.
    role: BrokerRole,
    /// What a Sidekick begins knowing of Memories, as they stood when the
    /// token was minted, and nothing for any other Agent.
    memories: MemoryIndex,
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

    /// The note the harness appends to its Agent's instructions, naming each
    /// Tool the Broker offers this Agent as `tool_name` spells the Tool the
    /// Broker serves under the given name. A Sidekick's names the Tools that
    /// are its alone and what it may not do, and the titles of the Memories
    /// most recently changed where there are any; every other Agent's is the
    /// same.
    pub fn instruction_note(&self, tool_name: impl Fn(&str) -> String) -> String {
        super::instruction_note(self.role, &self.memories, tool_name)
    }
}

#[cfg(test)]
impl BrokerHandoff {
    /// A handoff naming `endpoint` with a fresh token no Broker holds, for a
    /// test of how a harness lowers one onto its own seam.
    pub(crate) fn for_tests(endpoint: &str) -> Self {
        Self::for_tests_as(endpoint, BrokerRole::Agent)
    }

    /// As [`Self::for_tests`], handed to an Agent that is `role` to the
    /// Broker.
    pub(crate) fn for_tests_as(endpoint: &str, role: BrokerRole) -> Self {
        Self {
            endpoint: BrokerEndpoint(endpoint.to_owned()),
            token: BrokerToken::mint(),
            role,
            memories: MemoryIndex::default(),
        }
    }
}

/// The Session a Broker call is made for, and so the one every Tool acts for.
/// The token a request presented resolves it before any Tool runs, to the
/// Session that token names; a call that names the Agent making it more
/// exactly is then attributed to that Agent's Session
/// ([`BrokerCaller::attributed`]). This is the one answer a Tool has to "which
/// Session is calling", so none re-reads a header or a call's metadata, and
/// the one answer the Broker has to which Tools that caller is offered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrokerCaller {
    session_id: SessionId,
    role: BrokerRole,
}

impl BrokerCaller {
    pub(crate) fn session_id(self) -> SessionId {
        self.session_id
    }

    /// What the calling Agent is to the Broker.
    pub(crate) fn role(self) -> BrokerRole {
        self.role
    }

    /// Who a call presenting this caller's token is made for, given `agent`:
    /// the Provider's own identity for the Agent making the call, where the
    /// call names one — as a Codex thread names itself in every call it
    /// makes. A native Subagent rides its parent's Provider connection, and so
    /// presents the token that connection was handed; when `agent` names a
    /// native Subagent riding this token's connection, the call is that
    /// Subagent's. Anything else — no identity, or one naming no such
    /// Subagent — leaves the call the token's Session's, so a call's metadata
    /// can only narrow its caller to an Agent the token's connection serves,
    /// never reach past it (ADR 0035). A native Subagent is no top-level
    /// Session, so a call narrowed to one is no Sidekick's whatever its
    /// token's Session is.
    pub(crate) fn attributed(
        self,
        agent: Option<&ProviderSubagentId>,
        sessions: &SessionStore,
    ) -> Self {
        let Some(agent) = agent else {
            return self;
        };
        match sessions.native_subagent_riding(self.session_id, agent) {
            Some(session_id) => {
                tracing::debug!(
                    token_session_id = %self.session_id,
                    %session_id,
                    subagent = agent.as_str(),
                    "a Broker call is attributed to the native Subagent that made it"
                );
                Self {
                    session_id,
                    role: BrokerRole::Agent,
                }
            }
            None => self,
        }
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
    /// Whose Sessions' Agents are Sidekicks.
    sidekick: SidekickWorkspace,
    /// Where a Sidekick's handoff reads what it begins knowing of Memories.
    memories: MemoryStore,
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
    pub(crate) fn new(
        endpoint: String,
        settings: watch::Receiver<SettingsSnapshot>,
        sidekick: SidekickWorkspace,
        memories: MemoryStore,
    ) -> Self {
        Self {
            endpoint: BrokerEndpoint(endpoint),
            settings,
            sidekick,
            memories,
            live: Arc::default(),
        }
    }

    /// Whether the user has left the Broker on.
    pub(crate) fn is_enabled(&self) -> bool {
        self.settings.borrow().settings.broker.enabled
    }

    /// Mints a token naming `session` for one Provider connection, live
    /// until the returned grant is dropped, and fixes what its Agent is to the
    /// Broker from where `session` stands as it is started — and, for a
    /// Sidekick, what it begins knowing of Memories, from what the Server
    /// keeps now. Nothing while the Broker is off, so a Provider started then
    /// is handed no endpoint at all — a Sidekick's no more than any other.
    pub(crate) async fn grant(&self, session: &Session) -> Option<BrokerGrant> {
        if !self.is_enabled() {
            return None;
        }
        let session_id = session.id;
        let role = BrokerRole::of(session, &self.sidekick);
        let memories = match role {
            BrokerRole::Sidekick => self.memory_index().await,
            BrokerRole::Agent => MemoryIndex::default(),
        };
        let token = BrokerToken::mint();
        let id = {
            let mut live = self.live.lock().expect("Broker token lock is not poisoned");
            let id = live.next_id;
            live.next_id += 1;
            live.tokens.insert(
                id,
                LiveToken {
                    token: token.clone(),
                    caller: BrokerCaller { session_id, role },
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
                role,
                memories,
            },
        })
    }

    /// What a Sidekick started now begins knowing of Memories. Where the
    /// Server's own storage cannot say, the Log is told and the Sidekick is
    /// told nothing of Memories, as though there were none, rather than its
    /// start failing: it may still search for them.
    async fn memory_index(&self) -> MemoryIndex {
        self.memories.index().await.unwrap_or_else(|error| {
            tracing::warn!(
                "could not read the Memory index for a Sidekick's instructions: {error}"
            );
            MemoryIndex::default()
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
    use crate::{
        memories::{IndexedMemory, NewMemory},
        protocol::{BrokerSettings, EffectiveSettings, Workspace},
        storage::StorageRepository,
    };

    fn settings(enabled: bool) -> SettingsSnapshot {
        SettingsSnapshot {
            settings: EffectiveSettings {
                broker: BrokerSettings {
                    enabled,
                    ..BrokerSettings::default()
                },
                ..EffectiveSettings::default()
            },
            ..SettingsSnapshot::default()
        }
    }

    /// The Broker's access on a Server whose data root is a directory of its
    /// own, held for as long as the access is.
    struct Access {
        access: BrokerAccess,
        settings: watch::Sender<SettingsSnapshot>,
        sidekick: SidekickWorkspace,
        memories: MemoryStore,
        _data: tempfile::TempDir,
    }

    impl std::ops::Deref for Access {
        type Target = BrokerAccess;

        fn deref(&self) -> &BrokerAccess {
            &self.access
        }
    }

    async fn access(enabled: bool) -> Access {
        let (settings, receiver) = watch::channel(settings(enabled));
        let data = tempfile::tempdir().expect("create a data root");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let memories = MemoryStore::new(
            StorageRepository::open(data.path())
                .await
                .expect("open the data root's database"),
        );
        Access {
            access: BrokerAccess::new(
                "http://127.0.0.1:1/broker".to_owned(),
                receiver,
                sidekick.clone(),
                memories.clone(),
            ),
            settings,
            sidekick,
            memories,
            _data: data,
        }
    }

    /// A top-level Session working anywhere but the Sidekick Workspace.
    fn ordinary() -> Session {
        Session::for_tests(Workspace::directory(std::path::PathBuf::from("elsewhere")))
    }

    fn presenting(authorization: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(authorization).expect("a valid header value"),
        );
        headers
    }

    #[tokio::test]
    async fn a_granted_token_names_its_session_until_the_grant_is_dropped() {
        let access = access(true).await;
        let session = ordinary();
        let session_id = session.id;
        let grant = access.grant(&session).await.expect("the Broker is on");
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
            Some(BrokerCaller {
                session_id,
                role: BrokerRole::Agent
            }),
            "the authentication scheme is matched regardless of case"
        );
        drop(grant);
        assert_eq!(
            access.caller(&headers),
            None,
            "a retired token names no one"
        );
    }

    #[tokio::test]
    async fn every_grant_mints_a_token_of_its_own() {
        let access = access(true).await;
        let session = ordinary();
        let session_id = session.id;
        let first = access.grant(&session).await.expect("the Broker is on");
        let second = access.grant(&session).await.expect("the Broker is on");
        assert_ne!(first.handoff().token(), second.handoff().token());
        assert_eq!(first.handoff().endpoint(), second.handoff().endpoint());

        drop(first);
        assert_eq!(
            access.caller(&presenting(&second.handoff().token().bearer())),
            Some(BrokerCaller {
                session_id,
                role: BrokerRole::Agent
            }),
            "retiring one connection's token leaves the next one's live"
        );
    }

    #[tokio::test]
    async fn nothing_is_granted_while_the_broker_is_off() {
        let access = access(false).await;
        assert!(access.grant(&ordinary()).await.is_none());
        let sidekick = Session::for_tests(Workspace::directory(
            access
                .sidekick
                .ensure()
                .expect("make the Sidekick Workspace"),
        ));
        assert!(
            access.grant(&sidekick).await.is_none(),
            "a Sidekick's Session is handed nothing either"
        );
        access.settings.send_replace(settings(true));
        assert!(
            access.grant(&ordinary()).await.is_some(),
            "turning the Broker back on reaches the very next grant"
        );
    }

    #[tokio::test]
    async fn an_unknown_or_malformed_credential_names_no_one() {
        let access = access(true).await;
        let grant = access.grant(&ordinary()).await.expect("the Broker is on");
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

    #[tokio::test]
    async fn a_handoff_never_prints_its_token() {
        let access = access(true).await;
        let grant = access.grant(&ordinary()).await.expect("the Broker is on");
        let printed = format!("{grant:?}");
        assert!(!printed.contains(grant.handoff().token().secret()));
        assert!(printed.contains("http://127.0.0.1:1/broker"));
    }

    #[tokio::test]
    async fn a_top_level_session_of_the_sidekick_workspace_is_granted_as_a_sidekick() {
        let access = access(true).await;
        let root = access
            .sidekick
            .ensure()
            .expect("make the Sidekick Workspace");
        let sidekick = Session::for_tests(Workspace::directory(root.clone()));
        let subagent = Session {
            parent: Some(sidekick.id),
            ..Session::for_tests(Workspace::directory(root))
        };

        assert_eq!(
            granted_role(&access, &sidekick).await,
            Some(BrokerRole::Sidekick)
        );
        assert_eq!(
            granted_role(&access, &subagent).await,
            Some(BrokerRole::Agent),
            "a Sidekick's Subagent is no Sidekick, though it works in the same directory"
        );
        assert_eq!(
            granted_role(&access, &ordinary()).await,
            Some(BrokerRole::Agent)
        );
    }

    /// What `session`'s Agent is to the Broker, as the token a grant for it
    /// mints resolves.
    async fn granted_role(access: &Access, session: &Session) -> Option<BrokerRole> {
        let grant = access.grant(session).await.expect("the Broker is on");
        access
            .caller(&presenting(&grant.handoff().token().bearer()))
            .map(BrokerCaller::role)
    }

    /// A Sidekick's grant carries the titles of the Memories most recently
    /// changed as they stand when it is minted; any other Agent's carries
    /// none, though there are Memories to tell of.
    #[tokio::test]
    async fn a_sidekicks_grant_carries_the_memories_kept_as_it_is_minted_and_no_other_does() {
        let access = access(true).await;
        let root = access
            .sidekick
            .ensure()
            .expect("make the Sidekick Workspace");
        let sidekick = Session::for_tests(Workspace::directory(root));
        let memories = |grant: &BrokerGrant| grant.handoff().memories.clone();

        let before = access.grant(&sidekick).await.expect("the Broker is on");
        assert_eq!(memories(&before), MemoryIndex::default());
        let kept = access
            .memories
            .store(NewMemory {
                title: "How the user reviews".to_owned(),
                body: "Never squash.".to_owned(),
                tags: Vec::new(),
            })
            .await
            .expect("store a Memory");
        assert_eq!(
            memories(&before),
            MemoryIndex::default(),
            "a grant already minted is not told of a Memory stored since"
        );
        let after = access.grant(&sidekick).await.expect("the Broker is on");
        assert_eq!(
            memories(&after),
            MemoryIndex {
                recent: vec![IndexedMemory {
                    id: kept.id,
                    title: "How the user reviews".to_owned(),
                }],
                older: 0,
            }
        );
        let ordinary = access.grant(&ordinary()).await.expect("the Broker is on");
        assert_eq!(memories(&ordinary), MemoryIndex::default());
    }
}
