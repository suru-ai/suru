//! Identity providers: what a Relay asks who logged in (ADR-0046).
//!
//! Only the Relay ever speaks to one. A Server sees nothing of the provider's
//! but the address its user visits and the code they enter there, and the
//! Relay keeps nothing of the provider's but who logged in.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::admission::{AdmissionRule, Undecided};

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
    /// nobody did. The Relay stops waiting — dropping what this returns —
    /// once the login expires or the Server that began it goes, so it asks
    /// the provider nothing outside what this returns.
    async fn finish_login(&self, login: &DeviceLogin) -> Result<Identity, LoginRefusal>;

    /// Who goes by `name` at the provider now: their identity, or `None`
    /// where nobody does. The Relay asks as its operator's admission rules
    /// first name someone, and admits them by the identity it is told from
    /// then on, whoever comes to go by the name later.
    async fn look_up(&self, name: &str) -> Result<Option<Identity>, LookUpFailed>;

    /// The organization that goes by `name` at the provider now, by the
    /// provider's stable id for it, where the Relay can check who its members
    /// are; or why it cannot. The Relay asks as it starts, for each
    /// organization its operator's admission rules name, and admits the
    /// members of the organization it is told of the first time ever after,
    /// whatever comes to go by the name later. A provider with no
    /// organizations the Relay can check has none by any name.
    async fn look_up_organization(&self, name: &str) -> Result<String, OrganizationUnchecked> {
        Err(OrganizationUnchecked::Refused(format!(
            "{} has no organizations whose members this Relay can check, by `{name}` or any name",
            self.name()
        )))
    }

    /// Whether `identity` is a member of `organization` now, as the provider
    /// says without any token of the identity's: [`Undecided`] where it
    /// cannot say just now.
    async fn is_member(
        &self,
        organization: &Organization,
        _identity: &Identity,
    ) -> Result<bool, Undecided> {
        Err(Undecided(format!(
            "{} has no organizations whose members this Relay can check, `{}` among them",
            self.name(),
            organization.name
        )))
    }
}

/// An organization at an identity provider, as an admission rule names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Organization {
    /// The provider's stable id for it, by which it is known: never its name,
    /// which another organization may come to hold.
    pub id: String,
    /// The name the admission rules name it by.
    pub name: String,
}

/// Why the Relay cannot check the members of an organization its admission
/// rules name. The Relay writes it where its operator reads it, so it never
/// holds a token or a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrganizationUnchecked {
    /// The provider could not be asked just now — not reached, say, or
    /// limiting how often it is asked — saying why.
    Unavailable(String),
    /// The provider answered, and the organization's members cannot be
    /// checked, saying why: nothing goes by the name, say, or the Relay may
    /// not see them.
    Refused(String),
}

/// A login begun by device flow.
#[derive(Clone)]
pub struct DeviceLogin {
    /// Where the Server's user goes to log in.
    pub verification_uri: String,
    /// What they enter there.
    pub user_code: String,
    /// The provider's own handle for the login, which never leaves the Relay.
    pub device_code: String,
    /// How long the login may take before it expires.
    pub expires_in: Duration,
    /// How long the provider asks to be left between one asking after the
    /// login and the next.
    pub interval: Duration,
}

/// Leaves out the device code, which is the provider's handle for the login
/// and is never written anywhere.
impl std::fmt::Debug for DeviceLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceLogin")
            .field("verification_uri", &self.verification_uri)
            .field("user_code", &self.user_code)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
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

/// Why a provider could not say who goes by a name just now. The Relay
/// writes it where its operator reads it, so it never holds a token or a
/// secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LookUpFailed(pub String);

/// What a Relay logs in through while no identity provider is configured:
/// it logs nobody in, and knows nobody, and no organization, by name.
pub struct NoIdentityProvider;

/// Why a Relay with no identity provider can do nothing that needs one.
const NO_PROVIDER: &str = "this Relay has no identity provider configured";

#[async_trait]
impl IdentityProvider for NoIdentityProvider {
    fn name(&self) -> &str {
        "none"
    }

    async fn begin_login(&self) -> Result<DeviceLogin, LoginRefusal> {
        Err(LoginRefusal::Unavailable(NO_PROVIDER.to_owned()))
    }

    async fn finish_login(&self, _login: &DeviceLogin) -> Result<Identity, LoginRefusal> {
        Err(LoginRefusal::Unavailable(NO_PROVIDER.to_owned()))
    }

    async fn look_up(&self, _name: &str) -> Result<Option<Identity>, LookUpFailed> {
        Err(LookUpFailed(NO_PROVIDER.to_owned()))
    }
}

/// An identity provider whose logins a test finishes: each one begun waits
/// until the test approves or denies its user code, as the Server's user
/// would at a real provider. Given to a Relay as an admission rule as well,
/// it says whether each identity it logs in is admitted, as the test says:
/// each is, until the test says otherwise. Asked who goes by a name, it
/// answers as the test says, and knows nobody by name until it does.
pub struct ScriptedProvider {
    name: String,
    expires_in: Duration,
    logins: Mutex<ScriptedLogins>,
    admission: Mutex<ScriptedAdmission>,
    /// The subject each name is the name of, by name.
    names: Mutex<HashMap<String, String>>,
}

#[derive(Default)]
struct ScriptedAdmission {
    /// The subjects of the identities it no longer admits.
    refused: HashSet<String>,
    /// Whether it cannot tell whether anyone is admitted, as a provider
    /// that has stopped answering cannot.
    undecided: bool,
    /// How many times it has been asked.
    asked: u64,
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
            name: "scripted".to_owned(),
            expires_in: Duration::from_secs(15 * 60),
            logins: Mutex::default(),
            admission: Mutex::default(),
            names: Mutex::default(),
        }
    }

    /// Stands in for the identity provider named `provider`, logging in and
    /// admitting its identities as its own.
    pub fn standing_in_for(mut self, provider: &str) -> Self {
        provider.clone_into(&mut self.name);
        self
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

    /// Says from now on whether the identity `subject` is admitted, as an
    /// operator's rules would once its user came to satisfy them, or stopped.
    pub fn set_admitted(&self, subject: &str, admitted: bool) {
        let mut admission = self.admission();
        if admitted {
            admission.refused.remove(subject);
        } else {
            admission.refused.insert(subject.to_owned());
        }
    }

    /// Has it be unable to tell whether anyone is admitted while `undecided`
    /// holds, as a provider that has stopped answering would be.
    pub fn set_admission_undecided(&self, undecided: bool) {
        self.admission().undecided = undecided;
    }

    /// How many times a Relay has asked it whether someone is admitted.
    pub fn admissions_asked(&self) -> u64 {
        self.admission().asked
    }

    /// Says from now on that the identity `subject` goes by `name`, or, with
    /// no subject, that nobody does: as a user taking a name up, or giving it
    /// up, would at a real provider.
    pub fn set_name(&self, name: &str, subject: Option<&str>) {
        let mut names = self.names();
        match subject {
            Some(subject) => names.insert(name.to_owned(), subject.to_owned()),
            None => names.remove(name),
        };
    }

    fn names(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.names.lock().expect("scripted names are not poisoned")
    }

    fn admission(&self) -> std::sync::MutexGuard<'_, ScriptedAdmission> {
        self.admission
            .lock()
            .expect("scripted admission is not poisoned")
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
        &self.name
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
            interval: Duration::ZERO,
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

    async fn look_up(&self, name: &str) -> Result<Option<Identity>, LookUpFailed> {
        Ok(self.names().get(name).map(|subject| Identity {
            subject: subject.clone(),
            username: name.to_owned(),
        }))
    }
}

#[async_trait]
impl AdmissionRule for ScriptedProvider {
    async fn admits(&self, provider: &str, identity: &Identity) -> Result<bool, Undecided> {
        let mut admission = self.admission();
        admission.asked += 1;
        if admission.undecided {
            return Err(Undecided(
                "the scripted provider is not answering".to_owned(),
            ));
        }
        Ok(provider == self.name() && !admission.refused.contains(&identity.subject))
    }
}
