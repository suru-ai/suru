//! Admission: who may use a Relay, and what becomes of an Account once they
//! no longer may (ADR-0048).
//!
//! The Relay's operator sets the rules, and an identity any one of them
//! admits is admitted; a Relay with no rules admits nobody. The Relay asks
//! them as a Server's user logs in, before any Login is formed, and asks them
//! again for every Account a Login stands under as it starts and then on a
//! schedule of its own, without its Servers. An Account they no longer admit
//! lapses: every Login under it is refused, and everything standing on those
//! Logins — the connections their Servers hold, and the joins the Relay
//! carries for them — is cut at once, but nothing of it is forgotten, and one
//! fresh login from any Server of the Account, once the rules admit it again,
//! restores every Login under it. An operator may also require a fresh login
//! every so many days; that is off unless asked for, and an Account not
//! logged in as for longer lapses the same way and is restored the same way.
//!
//! A rule may name users. Each name is looked up at the Relay's identity
//! provider once, as the Relay first starts naming it, and the identity found
//! is kept and admitted by that name ever after, so a name its user gives up
//! admits nobody new once someone else takes it, however often the Relay
//! starts again, and whether or not the rules went on naming it meanwhile. A
//! name the rules no longer name admits nobody, and the Account it admitted
//! lapses as the Relay starts without it. The Relay refuses to start naming a
//! user the provider knows nobody by, or that it cannot look up just now,
//! rather than admit nobody by that name and say nothing — and a start it
//! refuses keeps nothing it looked up.
//!
//! A rule may name organizations, whose members it admits, as their identity
//! provider says each time it is asked, without any token of theirs. Each
//! organization is looked up as the Relay first starts naming it and kept by
//! the provider's stable id for it, so a name it gives up admits nobody new
//! once another organization takes it; and it is looked up again each time
//! the Relay starts, which refuses to start with an organization rule it
//! cannot check, saying which organization and why — its provider not
//! answering just then included, however often it was checked before. A
//! Relay already running keeps its Accounts while the provider cannot be
//! asked, as it does for any rule that cannot tell. A member who leaves the
//! organization is found to have left at the next check, and their Account
//! lapses then; an organization the rules no longer name admits nobody, and
//! the Accounts it alone admitted lapse as the Relay starts without it.
//!
//! A rule may be unable to tell just now — its identity provider not
//! answering, say, or not within the time the Relay gives it. An Account the
//! rules cannot tell about stands, since an identity provider that stops
//! answering would otherwise cut everyone off; and a login they cannot tell
//! about is refused until they can, so no one is admitted on nobody's word.
//!
//! Each pass of the schedule begins once the one before has ended and its
//! interval passed, so no two overlap however long the rules take, and
//! begins at the first Account the one before could not tell about, so rules
//! that can answer for only so many Accounts at a time — an identity provider
//! limiting how often it is asked — come to every Account in turn, whatever
//! other rules decide of the Accounts after it. The
//! rules are asked with nothing held that a Server's connection waits on: an
//! Account found no longer admitted lapses only afterwards, under the
//! standing lock, and only where no check begun later has admitted it since.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, SystemTime},
};

use anyhow::bail;
use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::{
    connection::Relay,
    identity::{Identity, IdentityProvider, LookUpFailed, Organization, OrganizationUnchecked},
    standing::Cut,
    store::Store,
};

/// A rule of who may use a Relay, set by its operator: one naming users,
/// say, or the members of an organization, or — on a public Relay — one
/// admitting anyone.
#[async_trait]
pub trait AdmissionRule: Send + Sync + 'static {
    /// Whether the rule admits `identity`, who logs in through the identity
    /// provider named `provider`; [`Undecided`] where it cannot tell just now.
    async fn admits(&self, provider: &str, identity: &Identity) -> Result<bool, Undecided>;
}

/// Why a rule could not tell whether it admits someone just now. The Relay
/// writes it to its diagnostic log, so it never holds a token or a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Undecided(pub String);

/// The rules a Relay admits by: an identity any one of them admits is
/// admitted, and with none, nobody is.
#[derive(Clone, Default)]
pub struct Admission {
    rules: Vec<Arc<dyn AdmissionRule>>,
    /// The users the rules name at the Relay's identity provider, as its
    /// operator wrote them, until the Relay starts and looks them up.
    named_users: Vec<String>,
    /// The organizations the rules name at the Relay's identity provider, as
    /// its operator wrote them, until the Relay starts and looks them up.
    organizations: Vec<String>,
}

/// What a Relay's rules found of someone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Admitted,
    NotAdmitted,
    /// No rule admits them and at least one could not tell, saying why.
    Undecided(String),
}

impl Admission {
    /// Rules admitting nobody, as a Relay has until its operator sets some.
    pub fn nobody() -> Self {
        Self::default()
    }

    /// Admits whoever any one of `rules` admits.
    pub fn by(rules: impl IntoIterator<Item = Arc<dyn AdmissionRule>>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
            named_users: Vec::new(),
            organizations: Vec::new(),
        }
    }

    /// Admits as well each user `names` names at the Relay's identity
    /// provider: whoever went by the name as the Relay first started naming
    /// them, by the provider's stable id for them, whatever they or anyone
    /// else go by afterwards, ever after. Names are told apart without regard
    /// to case, as GitHub's are.
    pub fn with_named_users(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.named_users.extend(names.into_iter().map(Into::into));
        self
    }

    /// Admits as well the members of each organization `names` names at the
    /// Relay's identity provider, as the provider says each time it is
    /// asked: of whichever organization went by the name as the Relay first
    /// started naming it, by the provider's stable id for it, whatever it or
    /// another organization goes by afterwards, ever after. Names are told
    /// apart without regard to case, as GitHub's are.
    pub fn with_organizations(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.organizations.extend(names.into_iter().map(Into::into));
        self
    }

    /// The rules with the users and the organizations they name looked up
    /// at `provider`: each by what `store` keeps for its name, where the
    /// Relay has started naming it before, or else as `provider` answers now,
    /// which `store` keeps from `now` on — and each organization asked after
    /// again, to see its members can still be checked. Fails, keeping nothing
    /// it looked up, so the Relay does not start, where a name is one
    /// `provider` knows nobody by, or cannot look up just now; where an
    /// organization's members cannot be checked, `provider` not answering
    /// just now included; or where an organization's name now names another
    /// than the one first found by it.
    pub(crate) async fn looked_up(
        mut self,
        store: &Store,
        provider: &Arc<dyn IdentityProvider>,
        now: SystemTime,
    ) -> anyhow::Result<Self> {
        let names = normalized(std::mem::take(&mut self.named_users), "a user")?;
        let organizations = normalized(std::mem::take(&mut self.organizations), "an organization")?;
        let at = provider.name();
        let kept = store
            .named_users(at, names.iter().cloned().collect())
            .await?;
        let mut found = Vec::new();
        for name in names.iter().filter(|name| !kept.contains_key(*name)) {
            let identity = match provider.look_up(name).await {
                Ok(Some(identity)) => identity,
                Ok(None) => bail!(
                    "the admission rules name the user `{name}`, and {at} knows nobody by that \
                     name"
                ),
                Err(LookUpFailed(why)) => bail!(
                    "the admission rules name the user `{name}`, and the Relay could not look \
                     them up at {at}: {why}. It looks each name up once, as it first starts \
                     naming them, and admits whoever went by it then ever after"
                ),
            };
            found.push((name.clone(), identity));
        }
        let kept_organizations = store
            .named_organizations(at, organizations.iter().cloned().collect())
            .await?;
        let mut found_organizations = Vec::new();
        let mut named_organizations = Vec::new();
        for name in &organizations {
            let first = kept_organizations.get(name);
            let id = match provider.look_up_organization(name).await {
                Ok(id) if first.is_some_and(|first| *first != id) => bail!(
                    "the admission rules name the organization `{name}`, and at {at} that name \
                     no longer names the organization the Relay first named by it, which it \
                     admits the members of ever after: the organization may have taken another \
                     name, and another organization this one. Name it by the name it goes by now"
                ),
                Ok(id) => {
                    if first.is_none() {
                        found_organizations.push((name.clone(), id.clone()));
                    }
                    id
                }
                Err(OrganizationUnchecked(why)) => bail!(
                    "the admission rules name the organization `{name}`, and the Relay cannot \
                     check its members at {at}: {why}"
                ),
            };
            named_organizations.push(Organization {
                id,
                name: name.clone(),
            });
        }
        store
            .keep_names(
                at,
                found
                    .iter()
                    .map(|(name, identity)| (name.clone(), identity.subject.clone()))
                    .collect(),
                found_organizations.clone(),
                now,
            )
            .await?;
        for (name, identity) in &found {
            tracing::info!(
                name,
                subject = identity.subject,
                username = identity.username,
                "a user the admission rules name was looked up, and is admitted as this \
                 identity by that name ever after"
            );
        }
        for (name, id) in &found_organizations {
            tracing::info!(
                name,
                id,
                "an organization the admission rules name was looked up, and its members are \
                 admitted by that name ever after"
            );
        }
        let subjects = kept
            .into_values()
            .chain(found.into_iter().map(|(_, identity)| identity.subject))
            .collect::<HashSet<_>>();
        if !subjects.is_empty() {
            self.rules.push(Arc::new(NamedUsers {
                provider: at.to_owned(),
                subjects,
            }));
        }
        for organization in named_organizations {
            self.rules.push(Arc::new(Members {
                provider: provider.clone(),
                organization,
            }));
        }
        Ok(self)
    }

    /// Asks every rule at once whether it admits `identity`, at the identity
    /// provider named `provider`, waiting no longer than `timeout` for them:
    /// admitted once any one does, whatever the rest have yet to say.
    pub(crate) async fn decide(
        &self,
        provider: &str,
        identity: &Identity,
        timeout: Duration,
    ) -> Verdict {
        let mut asking = self
            .rules
            .iter()
            .map(|rule| rule.admits(provider, identity))
            .collect::<FuturesUnordered<_>>();
        let deciding = tokio::time::timeout(timeout, async {
            let mut undecided = None;
            while let Some(answer) = asking.next().await {
                match answer {
                    Ok(true) => return Verdict::Admitted,
                    Ok(false) => {}
                    Err(Undecided(why)) => {
                        undecided.get_or_insert(why);
                    }
                }
            }
            undecided.map_or(Verdict::NotAdmitted, Verdict::Undecided)
        });
        deciding.await.unwrap_or_else(|_| {
            Verdict::Undecided(format!(
                "the admission rules did not answer within {timeout:?}"
            ))
        })
    }
}

/// `names`, as the admission rules name what they name — `kind` — told apart
/// without regard to case, refusing an empty one.
fn normalized(names: Vec<String>, kind: &str) -> anyhow::Result<BTreeSet<String>> {
    let names = names
        .iter()
        .map(|name| name.trim().to_lowercase())
        .collect::<BTreeSet<_>>();
    if names.contains("") {
        bail!("the admission rules name {kind} by an empty name");
    }
    Ok(names)
}

/// The rule naming users: it admits the identities at the identity provider
/// named `provider` that its names were found to be.
struct NamedUsers {
    provider: String,
    subjects: HashSet<String>,
}

#[async_trait]
impl AdmissionRule for NamedUsers {
    async fn admits(&self, provider: &str, identity: &Identity) -> Result<bool, Undecided> {
        Ok(provider == self.provider && self.subjects.contains(&identity.subject))
    }
}

/// The rule naming an organization: it admits the identities at `provider`
/// that `provider` says are its members, each time it is asked.
struct Members {
    provider: Arc<dyn IdentityProvider>,
    organization: Organization,
}

#[async_trait]
impl AdmissionRule for Members {
    async fn admits(&self, provider: &str, identity: &Identity) -> Result<bool, Undecided> {
        if provider != self.provider.name() {
            return Ok(false);
        }
        self.provider.is_member(&self.organization, identity).await
    }
}

/// One asking of the rules about one identity, numbered in the order the
/// askings began, so the verdict of one begun later is told apart from an
/// earlier one's however their answers come back. What orders the verdicts
/// about an identity is kept while any asking about it is under way, and
/// goes with the last of them, however it ends: taking effect, finding
/// nothing to do, or given up with the login it was asked for.
pub(crate) struct Check {
    number: u64,
    who: Who,
    ledger: Arc<Mutex<Ledger>>,
}

impl Check {
    /// The identity provider the identity asked about is at.
    pub(crate) fn provider(&self) -> &str {
        &self.who.0
    }

    /// The provider's stable id for the identity asked about.
    pub(crate) fn subject(&self) -> &str {
        &self.who.1
    }
}

impl Drop for Check {
    fn drop(&mut self) {
        lock(&self.ledger).ended(&self.who);
    }
}

/// Numbers each asking of the rules as it begins.
#[derive(Default)]
pub(crate) struct Checks(Arc<Mutex<Ledger>>);

impl Checks {
    /// Begins an asking of the rules about the identity `subject`, at the
    /// identity provider named `provider`.
    pub(crate) fn begin(&self, provider: &str, subject: &str) -> Check {
        let who = who(provider, subject);
        let mut ledger = lock(&self.0);
        let number = ledger.next;
        ledger.next += 1;
        *ledger.under_way.entry(who.clone()).or_default() += 1;
        Check {
            number,
            who,
            ledger: self.0.clone(),
        }
    }

    /// The verdicts its askings reach, for the standing lock to guard.
    pub(crate) fn verdicts(&self) -> Verdicts {
        Verdicts(self.0.clone())
    }

    /// How many identities anything is kept about.
    #[cfg(test)]
    fn identities_kept(&self) -> usize {
        let ledger = lock(&self.0);
        ledger
            .under_way
            .keys()
            .chain(ledger.admitted.keys())
            .chain(ledger.refused.keys())
            .collect::<HashSet<_>>()
            .len()
    }
}

/// An identity at an identity provider, as the rules are asked about it: the
/// provider's name, and the provider's stable id for the identity.
type Who = (String, String);

fn who(provider: &str, subject: &str) -> Who {
    (provider.to_owned(), subject.to_owned())
}

/// The askings of the rules under way, and what they found that took
/// effect, by identity.
#[derive(Default)]
struct Ledger {
    /// The number the next asking begun takes.
    next: u64,
    /// How many askings about each identity are under way.
    under_way: HashMap<Who, usize>,
    /// The latest asking that admitted each identity, and the latest that
    /// refused it, to take effect. Neither outranks an asking begun later, so
    /// they are kept only while one begun earlier may yet take effect: while
    /// any asking about the identity is under way.
    admitted: HashMap<Who, u64>,
    refused: HashMap<Who, u64>,
}

impl Ledger {
    /// Ends an asking about `who`, forgetting all that is kept about `who`
    /// where it was the last under way.
    fn ended(&mut self, who: &Who) {
        if let Some(under_way) = self.under_way.get_mut(who)
            && *under_way > 1
        {
            *under_way -= 1;
            return;
        }
        self.under_way.remove(who);
        self.admitted.remove(who);
        self.refused.remove(who);
    }
}

/// The ledger, as it stands. Each step taken under its lock is a few lookups
/// in maps, which leave it whole however they fail, so a step that panicked
/// leaves nothing to distrust.
fn lock(ledger: &Mutex<Ledger>) -> std::sync::MutexGuard<'_, Ledger> {
    ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Which asking of the rules last took effect for each identity, one way or
/// the other, so no verdict undoes one reached by an asking begun later: a
/// login they admitted forms or restores no Login once a later asking has
/// found them not to admit its identity, at a login or on the Relay's own
/// schedule, and a finding that they no longer admit it lapses nothing a
/// later asking admitted at login. It is kept by identity rather than by
/// Account, so a refusal counts as much for an identity that has no Account
/// yet, and only while an asking about that identity is under way. What the
/// standing lock guards: each verdict is weighed, and takes effect, under it.
#[derive(Default)]
pub(crate) struct Verdicts(Arc<Mutex<Ledger>>);

impl Verdicts {
    /// Whether `check` admitting its identity as it logs in may take effect.
    pub(crate) fn may_admit(&self, check: &Check) -> bool {
        lock(self.ledger(check))
            .refused
            .get(&check.who)
            .is_none_or(|refused| *refused < check.number)
    }

    pub(crate) fn admitted(&mut self, check: &Check) {
        let mut ledger = lock(self.ledger(check));
        let latest = ledger
            .admitted
            .entry(check.who.clone())
            .or_insert(check.number);
        *latest = (*latest).max(check.number);
    }

    /// Whether `check` finding the rules do not admit its identity may take
    /// effect.
    pub(crate) fn may_refuse(&self, check: &Check) -> bool {
        lock(self.ledger(check))
            .admitted
            .get(&check.who)
            .is_none_or(|admitted| *admitted < check.number)
    }

    pub(crate) fn refused(&mut self, check: &Check) {
        let mut ledger = lock(self.ledger(check));
        let latest = ledger
            .refused
            .entry(check.who.clone())
            .or_insert(check.number);
        *latest = (*latest).max(check.number);
    }

    /// The ledger `check` was begun in, which is this one.
    fn ledger<'a>(&'a self, check: &Check) -> &'a Mutex<Ledger> {
        debug_assert!(
            Arc::ptr_eq(&self.0, &check.ledger),
            "a check is weighed in the ledger it was begun in"
        );
        &self.0
    }
}

/// Why an Account lapses.
enum Lapse {
    /// The rules no longer admit the identity that logs in as it.
    NotAdmitted,
    /// Its operator requires a fresh login every so often, and none of its
    /// Servers has logged in as it since `logged_in_at`, as it was read, in
    /// seconds since the Unix epoch.
    LoginDue { logged_in_at: i64 },
}

/// Checks every Account a Login stands under every `relay`'s admission
/// interval, until what awaits this is dropped, after the check the Relay
/// makes as it starts. The interval runs from the end of one pass to the
/// beginning of the next, so no two overlap. It bounds how long an Account
/// the rules stop admitting goes on standing only together with how long a
/// pass takes: a pass asks about its Accounts one after another, each for no
/// longer than the Relay's admission timeout, so while the rules answer
/// nothing a pass over N Accounts takes N such timeouts.
pub(crate) async fn keep_checking(relay: &Relay) {
    loop {
        tokio::time::sleep(relay.admission_interval).await;
        if let Err(error) = check_every_account(relay).await {
            tracing::error!(
                "the Relay could not check its Accounts against its admission rules, as its \
                 records could not be used: {error:#}"
            );
        }
    }
}

/// Lapses each Account a Login stands under that is due a fresh login, or
/// that the rules no longer admit, one Account after another: those the
/// rules cannot tell about stand. It begins at the first Account the check
/// before it could not tell about, and comes round to those before it last,
/// so an Account the rules could not tell about is asked about first next
/// time, whatever they decided of those after it.
pub(crate) async fn check_every_account(relay: &Relay) -> anyhow::Result<()> {
    let fresh_since = relay.fresh_since();
    let mut undecided = 0_usize;
    let mut why_undecided = None;
    let mut accounts = relay.store.standing_accounts().await?;
    let resume_at = relay.resume_at.load(Ordering::Relaxed);
    accounts.sort_by_key(|account| (account.id < resume_at, account.id));
    let mut first_undecided = None;
    for account in accounts {
        if account.is_due(fresh_since) {
            let standing = relay.standing.lock().await;
            let due = Lapse::LoginDue {
                logged_in_at: account.logged_in_at,
            };
            lapse(relay, &standing, account.id, due).await?;
            continue;
        }
        let check = relay
            .checks
            .begin(&account.provider, &account.identity.subject);
        match relay
            .admission
            .decide(
                &account.provider,
                &account.identity,
                relay.admission_timeout,
            )
            .await
        {
            Verdict::Admitted => {}
            Verdict::NotAdmitted => refuse(relay, &check).await?,
            Verdict::Undecided(why) => {
                undecided += 1;
                why_undecided.get_or_insert(why);
                first_undecided.get_or_insert(account.id);
            }
        }
    }
    relay
        .resume_at
        .store(first_undecided.unwrap_or(0), Ordering::Relaxed);
    if let Some(why) = why_undecided {
        tracing::warn!(
            "the admission rules could not tell whether they still admit {undecided} Accounts, \
             which stand until they can: {why}"
        );
    }
    Ok(())
}

/// Takes effect, under the standing lock, of `check` finding that the rules
/// do not admit the identity it asked about — found at a login or on the
/// Relay's own schedule alike — unless an asking begun later has admitted it
/// since: from then on no admission reached by an asking begun earlier takes
/// effect for it, and the Account it answers to, where it answers to one
/// that stands, lapses at once. No other Account is touched, whatever Server
/// the finding came of.
pub(crate) async fn refuse(relay: &Relay, check: &Check) -> anyhow::Result<()> {
    let mut verdicts = relay.standing.lock().await;
    if !verdicts.may_refuse(check) {
        return Ok(());
    }
    verdicts.refused(check);
    match relay
        .store
        .account_answering(check.provider(), check.subject())
        .await?
    {
        Some(account) => lapse(relay, &verdicts, account, Lapse::NotAdmitted).await,
        None => Ok(()),
    }
}

/// Lapses the Account `account` for `why`, while the standing lock is held,
/// as `standing` shows — unless it has lapsed already, or one of its Servers
/// has logged in afresh since it was found due: every Login under it is
/// refused from then on, and everything standing on them is cut at once.
/// Nothing of it is forgotten.
async fn lapse(relay: &Relay, standing: &Verdicts, account: i64, why: Lapse) -> anyhow::Result<()> {
    let unless_logged_in_since = match why {
        Lapse::NotAdmitted => None,
        Lapse::LoginDue { logged_in_at } => Some(logged_in_at),
    };
    let Some(keys) = relay
        .store
        .lapse(account, relay.clock.now(), unless_logged_in_since)
        .await?
    else {
        return Ok(());
    };
    relay.cut(standing, &keys, Cut::Refused);
    match why {
        Lapse::NotAdmitted => tracing::info!(
            account,
            "an Account lapsed: the admission rules no longer admit it"
        ),
        Lapse::LoginDue { .. } => tracing::info!(
            account,
            "an Account lapsed: none of its Servers has logged in as it as recently as this \
             Relay requires"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn identity(subject: &str) -> Identity {
        Identity {
            subject: subject.to_owned(),
            username: format!("user-{subject}"),
        }
    }

    /// A rule answering for each subject as a test says, and admitting no
    /// one else.
    struct Answering(Mutex<HashMap<String, Result<bool, Undecided>>>);

    impl Answering {
        fn rule(answers: &[(&str, Result<bool, Undecided>)]) -> Arc<dyn AdmissionRule> {
            Arc::new(Self(Mutex::new(
                answers
                    .iter()
                    .map(|(subject, answer)| ((*subject).to_owned(), answer.clone()))
                    .collect(),
            )))
        }
    }

    #[async_trait]
    impl AdmissionRule for Answering {
        async fn admits(&self, _provider: &str, identity: &Identity) -> Result<bool, Undecided> {
            self.0
                .lock()
                .unwrap()
                .get(&identity.subject)
                .cloned()
                .unwrap_or(Ok(false))
        }
    }

    /// A rule that never answers.
    struct Silent;

    #[async_trait]
    impl AdmissionRule for Silent {
        async fn admits(&self, _provider: &str, _identity: &Identity) -> Result<bool, Undecided> {
            std::future::pending().await
        }
    }

    const PATIENCE: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_relay_with_no_rules_admits_nobody() {
        assert_eq!(
            Admission::nobody()
                .decide("github", &identity("17"), PATIENCE)
                .await,
            Verdict::NotAdmitted
        );
    }

    #[tokio::test]
    async fn anyone_one_rule_admits_is_admitted_whatever_the_others_say() {
        let unsure = Undecided("the provider did not answer".to_owned());
        let admission = Admission::by([
            Answering::rule(&[("17", Err(unsure.clone())), ("99", Ok(false))]),
            Answering::rule(&[("17", Ok(true))]),
            Arc::new(Silent),
        ]);
        assert_eq!(
            admission.decide("github", &identity("17"), PATIENCE).await,
            Verdict::Admitted,
            "admitted by one rule though another cannot tell and a third never answers"
        );

        let admission = Admission::by([
            Answering::rule(&[("17", Err(unsure.clone()))]),
            Answering::rule(&[("17", Ok(false))]),
        ]);
        assert_eq!(
            admission.decide("github", &identity("17"), PATIENCE).await,
            Verdict::Undecided(unsure.0),
            "none admits, and one cannot tell"
        );
        assert_eq!(
            admission.decide("github", &identity("99"), PATIENCE).await,
            Verdict::NotAdmitted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rules_that_do_not_answer_in_time_cannot_tell() {
        let admission = Admission::by([Arc::new(Silent) as Arc<dyn AdmissionRule>]);
        assert!(matches!(
            admission
                .decide("github", &identity("17"), Duration::from_secs(5))
                .await,
            Verdict::Undecided(_)
        ));
    }

    /// The shape admission is built to: a rule that admits anyone, as a
    /// public Relay would have, is one more rule, and asks nothing else of
    /// the Relay.
    #[tokio::test]
    async fn a_rule_admitting_anyone_is_one_more_rule() {
        struct Anyone;

        #[async_trait]
        impl AdmissionRule for Anyone {
            async fn admits(&self, _: &str, _: &Identity) -> Result<bool, Undecided> {
                Ok(true)
            }
        }

        let admission = Admission::by([Arc::new(Anyone) as Arc<dyn AdmissionRule>]);
        for (provider, subject) in [("github", "17"), ("okta", "00u1"), ("scripted", "x")] {
            assert_eq!(
                admission
                    .decide(provider, &identity(subject), PATIENCE)
                    .await,
                Verdict::Admitted
            );
        }
    }

    #[test]
    fn no_verdict_undoes_one_reached_by_an_asking_begun_later() {
        let checks = Checks::default();
        let mut verdicts = checks.verdicts();
        let first = checks.begin("github", "17");
        let (another, elsewhere) = (checks.begin("github", "99"), checks.begin("okta", "17"));
        let (second, third) = (checks.begin("github", "17"), checks.begin("github", "17"));
        assert!(first.number < second.number && second.number < third.number);

        // A login admitted by an asking begun before one that refused its
        // identity forms or restores nothing; one begun after does.
        verdicts.refused(&second);
        assert!(!verdicts.may_admit(&first));
        assert!(verdicts.may_admit(&third));
        assert!(
            verdicts.may_admit(&another) && verdicts.may_admit(&elsewhere),
            "another identity is its own"
        );

        // An asking that refuses an identity takes effect only where no
        // asking begun later has admitted it at login.
        let checks = Checks::default();
        let mut verdicts = checks.verdicts();
        let (first, second, third) = (
            checks.begin("github", "17"),
            checks.begin("github", "17"),
            checks.begin("github", "17"),
        );
        verdicts.admitted(&second);
        assert!(!verdicts.may_refuse(&first));
        assert!(verdicts.may_refuse(&third));
        verdicts.admitted(&first);
        assert!(
            !verdicts.may_refuse(&first) && verdicts.may_refuse(&third),
            "an earlier admission recorded late does not set the latest back"
        );
    }

    /// Identities refused one after another — as anyone who can log in at
    /// the provider can be — leave nothing behind, and one whose refusal
    /// must yet outrank an asking begun before it is kept only until that
    /// asking ends, however it ends.
    #[test]
    fn what_orders_an_identitys_verdicts_is_kept_only_while_an_asking_about_it_is_under_way() {
        let checks = Checks::default();
        let mut verdicts = checks.verdicts();
        for subject in 0..1_000 {
            let check = checks.begin("github", &subject.to_string());
            assert!(verdicts.may_refuse(&check));
            verdicts.refused(&check);
        }
        assert_eq!(checks.identities_kept(), 0);

        let earlier = checks.begin("github", "17");
        let refusing = checks.begin("github", "17");
        verdicts.refused(&refusing);
        drop(refusing);
        assert!(
            !verdicts.may_admit(&earlier),
            "the refusal still outranks it"
        );
        assert_eq!(checks.identities_kept(), 1);
        // Given up, as an asking is with the login it was asked for once its
        // Server goes.
        drop(earlier);
        assert_eq!(checks.identities_kept(), 0);
        assert!(verdicts.may_admit(&checks.begin("github", "17")));
    }
}
