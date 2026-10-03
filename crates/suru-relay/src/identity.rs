//! Identity providers: what a Relay asks who logged in (ADR-0046).
//!
//! Only the Relay ever speaks to one. A Server sees nothing of the provider's
//! but the address its user visits and the code they enter there, and the
//! Relay keeps nothing of the provider's but who logged in.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use tokio::sync::oneshot;

/// An identity provider a Relay logs Servers' users in through, by device
/// flow: the user visits an address on any device and enters a short code
/// there, so a Server on a machine with no browser can log in.
#[async_trait]
pub trait IdentityProvider: Send + Sync + 'static {
    /// The provider's stable name. With an identity's subject it keys that
    /// identity, so it never changes once anyone has logged in through it.
    fn name(&self) -> &str;

    /// Begins a login.
    async fn begin_login(&self) -> Result<DeviceLogin, LoginRefusal>;

    /// Waits for the login begun as `login` to end: who logged in, or why
    /// nobody did. The Relay stops waiting once the login expires.
    async fn finish_login(&self, login: &DeviceLogin) -> Result<Identity, LoginRefusal>;
}

/// A login begun by device flow.
#[derive(Clone, Debug)]
pub struct DeviceLogin {
    /// Where the Server's user goes to log in.
    pub verification_uri: String,
    /// What they enter there.
    pub user_code: String,
    /// The provider's own handle for the login, which never leaves the Relay.
    pub device_code: String,
    /// How long the login may take before it expires.
    pub expires_in: Duration,
}

/// Who logged in, as the identity provider knows them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    /// The provider's stable id for this identity, by which it is known: never
    /// a name or an address, which another person may come to hold.
    pub subject: String,
    /// The identity's name at the provider, kept as a label only.
    pub username: String,
}

/// Why nobody logged in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginRefusal {
    /// The user refused the login at the provider.
    Denied,
    /// Nobody finished it in time.
    Expired,
    /// The provider could not be asked, saying why.
    Unavailable(String),
}

/// What a Relay logs in through while no identity provider is configured:
/// it logs nobody in.
pub struct NoIdentityProvider;

#[async_trait]
impl IdentityProvider for NoIdentityProvider {
    fn name(&self) -> &str {
        "none"
    }

    async fn begin_login(&self) -> Result<DeviceLogin, LoginRefusal> {
        Err(LoginRefusal::Unavailable(
            "this Relay has no identity provider configured".to_owned(),
        ))
    }

    async fn finish_login(&self, _login: &DeviceLogin) -> Result<Identity, LoginRefusal> {
        Err(LoginRefusal::Unavailable(
            "this Relay has no identity provider configured".to_owned(),
        ))
    }
}

/// An identity provider whose logins a test finishes: each one begun waits
/// until the test approves or denies its user code, as the Server's user
/// would at a real provider.
pub struct ScriptedProvider {
    expires_in: Duration,
    logins: Mutex<ScriptedLogins>,
}

#[derive(Default)]
struct ScriptedLogins {
    begun: u64,
    /// How each login begun and not yet decided ends, by its user code.
    deciding: HashMap<String, oneshot::Sender<Result<Identity, LoginRefusal>>>,
    /// What each login begun and not yet waited on is told, by its device
    /// code.
    awaiting: HashMap<String, oneshot::Receiver<Result<Identity, LoginRefusal>>>,
}

/// The address a [`ScriptedProvider`] sends its users to.
pub const SCRIPTED_VERIFICATION_URI: &str = "https://login.scripted.invalid/device";

impl ScriptedProvider {
    pub fn new() -> Self {
        Self {
            expires_in: Duration::from_secs(15 * 60),
            logins: Mutex::default(),
        }
    }

    /// Lets each login run only `expires_in` before it expires.
    pub fn with_expiry(mut self, expires_in: Duration) -> Self {
        self.expires_in = expires_in;
        self
    }

    /// Logs in as `identity` the login whose user code is `user_code`,
    /// answering whether one was waiting.
    pub fn approve(&self, user_code: &str, identity: Identity) -> bool {
        self.decide(user_code, Ok(identity))
    }

    /// Refuses the login whose user code is `user_code`, answering whether
    /// one was waiting.
    pub fn deny(&self, user_code: &str) -> bool {
        self.decide(user_code, Err(LoginRefusal::Denied))
    }

    fn decide(&self, user_code: &str, outcome: Result<Identity, LoginRefusal>) -> bool {
        self.logins
            .lock()
            .expect("scripted logins are not poisoned")
            .deciding
            .remove(user_code)
            .is_some_and(|decide| decide.send(outcome).is_ok())
    }
}

impl Default for ScriptedProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IdentityProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn begin_login(&self) -> Result<DeviceLogin, LoginRefusal> {
        let mut logins = self
            .logins
            .lock()
            .expect("scripted logins are not poisoned");
        logins.begun += 1;
        let user_code = format!("CODE-{:04}", logins.begun);
        let device_code = format!("device-{}", logins.begun);
        let (decide, outcome) = oneshot::channel();
        logins.deciding.insert(user_code.clone(), decide);
        logins.awaiting.insert(device_code.clone(), outcome);
        Ok(DeviceLogin {
            verification_uri: SCRIPTED_VERIFICATION_URI.to_owned(),
            user_code,
            device_code,
            expires_in: self.expires_in,
        })
    }

    async fn finish_login(&self, login: &DeviceLogin) -> Result<Identity, LoginRefusal> {
        let outcome = self
            .logins
            .lock()
            .expect("scripted logins are not poisoned")
            .awaiting
            .remove(&login.device_code);
        match outcome {
            Some(outcome) => outcome.await.unwrap_or(Err(LoginRefusal::Expired)),
            None => Err(LoginRefusal::Unavailable(
                "this login is not one the scripted provider began".to_owned(),
            )),
        }
    }
}
