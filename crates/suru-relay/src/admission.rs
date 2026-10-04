//! Admission: who may use a Relay, and what becomes of an Account once they
//! no longer may (ADR-0048).
//!
//! The Relay's operator sets the rules, and an identity any one of them
//! admits is admitted; a Relay with no rules admits nobody. The Relay asks
//! them as a Server's user logs in, before any Login is formed, and asks them
//! again on a schedule of its own for every Account a Login stands under,
//! without its Servers. An Account they no longer admit lapses: every Login
//! under it is refused, and everything standing on those Logins — the
//! connections their Servers hold, and the joins the Relay carries for them —
//! is cut at once, but nothing of it is forgotten, and one fresh login from
//! any Server of the Account, once the rules admit it again, restores every
//! Login under it. An operator may also require a fresh login every so many
//! days; that is off unless asked for, and an Account not logged in as for
//! longer lapses the same way and is restored the same way.
//!
//! A rule may be unable to tell just now — its identity provider not
//! answering, say, or not within the time the Relay gives it. An Account the
//! rules cannot tell about stands, since an identity provider that stops
//! answering would otherwise cut everyone off; and a login they cannot tell
//! about is refused until they can, so no one is admitted on nobody's word.
//!
//! Each pass of the schedule begins once the one before has ended and its
//! interval passed, so no two overlap however long the rules take, and the
//! rules are asked with nothing held that a Server's connection waits on: an
//! Account found no longer admitted lapses only afterwards, under the
//! standing lock, and only where no check begun later has admitted it since.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::{connection::Relay, identity::Identity, standing::Cut};

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
        }
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

/// One asking of the rules, numbered in the order the askings began, so the
/// verdict of one begun later is told apart from an earlier one's however
/// their answers come back.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Check(u64);

/// Numbers each asking of the rules as it begins.
#[derive(Default)]
pub(crate) struct Checks(AtomicU64);

impl Checks {
    pub(crate) fn begin(&self) -> Check {
        Check(self.0.fetch_add(1, Ordering::AcqRel))
    }
}

/// An identity at an identity provider, as the rules are asked about it: the
/// provider's name, and the provider's stable id for the identity.
type Who = (String, String);

fn who(provider: &str, subject: &str) -> Who {
    (provider.to_owned(), subject.to_owned())
}

/// Which asking of the rules last took effect for each identity, one way or
/// the other, so no verdict undoes one reached by an asking begun later: a
/// login they admitted forms or restores no Login once a later asking has
/// found them not to admit its identity, at a login or on the Relay's own
/// schedule, and a finding that they no longer admit it lapses nothing a
/// later asking admitted at login. It is kept by identity rather than by
/// Account, so a refusal counts as much for an identity that has no Account
/// yet. What the standing lock guards.
#[derive(Default)]
pub(crate) struct Verdicts {
    admitted: HashMap<Who, Check>,
    refused: HashMap<Who, Check>,
}

impl Verdicts {
    /// Whether `check` admitting the identity `subject`, at the identity
    /// provider named `provider`, as it logs in may take effect.
    pub(crate) fn may_admit(&self, provider: &str, subject: &str, check: Check) -> bool {
        self.refused
            .get(&who(provider, subject))
            .is_none_or(|refused| *refused < check)
    }

    pub(crate) fn admitted(&mut self, provider: &str, subject: &str, check: Check) {
        let latest = self.admitted.entry(who(provider, subject)).or_insert(check);
        *latest = (*latest).max(check);
    }

    /// Whether `check` finding the rules do not admit the identity
    /// `subject`, at the identity provider named `provider`, may take effect.
    pub(crate) fn may_refuse(&self, provider: &str, subject: &str, check: Check) -> bool {
        self.admitted
            .get(&who(provider, subject))
            .is_none_or(|admitted| *admitted < check)
    }

    pub(crate) fn refused(&mut self, provider: &str, subject: &str, check: Check) {
        let latest = self.refused.entry(who(provider, subject)).or_insert(check);
        *latest = (*latest).max(check);
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

/// Checks every Account a Login stands under, every `relay`'s admission
/// interval, until what awaits this is dropped. The interval runs from the
/// end of one pass to the beginning of the next, so no two overlap. It bounds
/// how long an Account the rules stop admitting goes on standing only
/// together with how long a pass takes: a pass asks about its Accounts one
/// after another, each for no longer than the Relay's admission timeout, so
/// while the rules answer nothing a pass over N Accounts takes N such
/// timeouts.
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
/// that the rules no longer admit, one Account after another.
async fn check_every_account(relay: &Relay) -> anyhow::Result<()> {
    let fresh_since = relay.fresh_since();
    let mut undecided = 0_usize;
    let mut why_undecided = None;
    for account in relay.store.standing_accounts().await? {
        if account.is_due(fresh_since) {
            let standing = relay.standing.lock().await;
            let due = Lapse::LoginDue {
                logged_in_at: account.logged_in_at,
            };
            lapse(relay, &standing, account.id, due).await?;
            continue;
        }
        let check = relay.checks.begin();
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
            Verdict::NotAdmitted => {
                refuse(relay, &account.provider, &account.identity.subject, check).await?;
            }
            Verdict::Undecided(why) => {
                undecided += 1;
                why_undecided.get_or_insert(why);
            }
        }
    }
    if let Some(why) = why_undecided {
        tracing::warn!(
            "the admission rules could not tell whether they still admit {undecided} Accounts, \
             which stand until they can: {why}"
        );
    }
    Ok(())
}

/// Takes effect, under the standing lock, of `check` finding that the rules
/// do not admit the identity `subject`, at the identity provider named
/// `provider` — found at a login or on the Relay's own schedule alike —
/// unless an asking begun later has admitted it since: from then on no
/// admission reached by an asking begun earlier takes effect for it, and the
/// Account it answers to, where it answers to one that stands, lapses at
/// once. No other Account is touched, whatever Server the finding came of.
pub(crate) async fn refuse(
    relay: &Relay,
    provider: &str,
    subject: &str,
    check: Check,
) -> anyhow::Result<()> {
    let mut verdicts = relay.standing.lock().await;
    if !verdicts.may_refuse(provider, subject, check) {
        return Ok(());
    }
    verdicts.refused(provider, subject, check);
    match relay.store.account_answering(provider, subject).await? {
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
        let (first, second, third) = (checks.begin(), checks.begin(), checks.begin());
        assert!(first < second && second < third);

        // A login admitted by an asking begun before one that refused its
        // identity forms or restores nothing; one begun after does.
        let mut verdicts = Verdicts::default();
        verdicts.refused("github", "17", second);
        assert!(!verdicts.may_admit("github", "17", first));
        assert!(verdicts.may_admit("github", "17", third));
        assert!(
            verdicts.may_admit("github", "99", first) && verdicts.may_admit("okta", "17", first),
            "another identity is its own"
        );

        // An asking that refuses an identity takes effect only where no
        // asking begun later has admitted it at login.
        let mut verdicts = Verdicts::default();
        verdicts.admitted("github", "17", second);
        assert!(!verdicts.may_refuse("github", "17", first));
        assert!(verdicts.may_refuse("github", "17", third));
        verdicts.admitted("github", "17", first);
        assert!(
            !verdicts.may_refuse("github", "17", first)
                && verdicts.may_refuse("github", "17", third),
            "an earlier admission recorded late does not set the latest back"
        );
    }
}
