//! Running Suru's own Provider work as Errands.
//!
//! An Errand is one Provider call Suru makes for its own purposes rather than
//! the user's: one Prompt in, one reply shaped by a schema, carrying no Tools
//! and belonging to no Session. This module owns everything true of *every*
//! Errand — which runtime runs it, how long it may take, that a server
//! shutting down abandons it, and that each one reaching a Provider is
//! recorded with the Usage and Cost it consumed — and nothing about what any
//! particular Errand is for beyond naming its purpose. Deriving Titles and
//! Workspace Descriptions are the only callers today; compaction and
//! summarization are meant to arrive here unchanged (ADR 0011, ADR 0057).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::watch, time::Duration};

use crate::{
    clock::ServerClock,
    protocol::{ModelId, ProviderId, SessionTimestamp, SettingsSnapshot, Usage},
    provider::{ErrandConsumption, ErrandCost, ProviderErrand, ProviderRuntime, wait_for_shutdown},
};

/// How long an Errand may take before Suru stops waiting. Generous next to any
/// real one-shot call, and short enough that a wedged Provider cannot leave
/// work outstanding indefinitely. Injectable through
/// [`ErrandRunner::with_timeout`] so tests inject millisecond-scale values
/// rather than sleeping.
pub(crate) const DEFAULT_ERRAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an Errand produced no answer. Every variant is a Log line and nothing
/// more: whatever asked for an Errand always has something to fall back on.
#[derive(Debug)]
pub(crate) enum ErrandFailure {
    /// The Errand named a Provider this server does not host.
    ProviderNotHosted,
    /// The Errand named a Provider the user has turned off. A disabled Provider
    /// is one Suru leaves entirely alone, so it is not asked and Suru does not
    /// walk to another in its place.
    ProviderDisabled,
    /// The Provider did not answer within the Errand's timeout.
    TimedOut,
    /// The server began shutting down. The Errand is abandoned outright — never
    /// retried, never resumed on the next start.
    Abandoned,
    Provider(String),
}

impl std::fmt::Display for ErrandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderNotHosted => formatter.write_str("its Provider is not hosted here"),
            Self::ProviderDisabled => formatter.write_str("its Provider is turned off"),
            Self::TimedOut => formatter.write_str("the Provider did not answer in time"),
            Self::Abandoned => formatter.write_str("the server is shutting down"),
            Self::Provider(message) => formatter.write_str(message),
        }
    }
}

/// What an Errand was run for, as its record names it. Storage spells it as
/// serde does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrandPurpose {
    /// A Session's Title, derived of Suru's own accord with its Icon and
    /// branch name.
    SessionTitle,
    /// A Title proposed at the user's asking.
    TitleProposal,
    /// A Workspace's Description, derived with its Icon.
    WorkspaceDescription,
}

/// How an Errand that reached its Provider ended. Storage spells it as serde
/// does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrandOutcome {
    /// The Provider answered with a reply. The runner never checks a reply
    /// against the Errand's schema — whoever asked for the Errand does — but a
    /// Provider that can tell its reply fell outside the schema, as Claude's
    /// CLI can, fails the Errand instead.
    Answered,
    /// The Provider said it could not answer, or answered in a shape it
    /// could tell was not the one asked for.
    Failed,
    /// Suru stopped waiting at the Errand's deadline.
    TimedOut,
    /// Suru stopped waiting before the deadline: the server began shutting
    /// down, or whoever asked for the Errand stopped waiting for it.
    Abandoned,
}

/// What Suru keeps of one Errand that reached a Provider beside its answer
/// (ADR 0057): what it was for, where it ran, when, how it ended, and what it
/// consumed. Its Usage, and its Cost with the Basis that computed it, are each
/// absent where unknown — always so for an Errand that timed out or was
/// abandoned, since Suru stopped listening before the Provider could say. A
/// failed Errand keeps whatever its Provider could still say it consumed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ErrandRecord {
    pub(crate) purpose: ErrandPurpose,
    pub(crate) provider: ProviderId,
    pub(crate) model: ModelId,
    pub(crate) began: SessionTimestamp,
    pub(crate) ended: SessionTimestamp,
    pub(crate) outcome: ErrandOutcome,
    pub(crate) usage: Option<Usage>,
    pub(crate) cost: Option<ErrandCost>,
}

/// Where the runner keeps its Errand records. Taking a record never waits for
/// it to be kept, so recording never holds up an Errand's answer, and a record
/// refused is the runner's to log rather than the answer's to carry.
pub(crate) trait ErrandLedger: Send + Sync {
    fn record(&self, record: ErrandRecord) -> Result<(), RecordRefused>;
}

/// Why a ledger would not take an Errand's record, for the Log.
#[derive(Debug)]
pub(crate) struct RecordRefused(pub(crate) String);

impl std::fmt::Display for RecordRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone)]
pub(crate) struct ErrandRunner {
    /// Every Provider runtime this server hosts. An Errand routes to the
    /// runtime its Agent Selection names and to no other: Suru never walks to
    /// another Provider to get its own work done.
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    timeout: Duration,
    shutdown: watch::Receiver<bool>,
    /// The effective Settings in force, read when an Errand is about to run
    /// rather than when the Errand was asked for, so a Provider turned off
    /// mid-derivation is one Suru still does not call.
    settings: watch::Receiver<SettingsSnapshot>,
    /// Where each Errand that reached a Provider is recorded.
    ledger: Arc<dyn ErrandLedger>,
    /// Where an Errand's beginning and end are read.
    clock: ServerClock,
}

impl ErrandRunner {
    pub(crate) fn new(
        runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
        shutdown: watch::Receiver<bool>,
        settings: watch::Receiver<SettingsSnapshot>,
        ledger: Arc<dyn ErrandLedger>,
    ) -> Self {
        Self {
            runtimes,
            timeout: DEFAULT_ERRAND_TIMEOUT,
            shutdown,
            settings,
            ledger,
            clock: ServerClock::default(),
        }
    }

    /// Bounds how long an Errand may take; injectable so tests exercise the
    /// deadline without waiting out the default.
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reads when each Errand began and ended from `clock`.
    pub(crate) fn with_clock(mut self, clock: ServerClock) -> Self {
        self.clock = clock;
        self
    }

    /// Runs one Errand for `purpose` and answers with the JSON the Provider
    /// replied. The reply is unvalidated: the schema is a request rather than
    /// a guarantee, so the caller checks the shape itself.
    ///
    /// An Errand that reaches its Provider is recorded however it ends —
    /// including when whoever asked for it stops waiting — and one that never
    /// does is not recorded at all.
    pub(crate) async fn run(
        &self,
        purpose: ErrandPurpose,
        errand: ProviderErrand,
    ) -> Result<Value, ErrandFailure> {
        let Some(runtime) = self
            .runtimes
            .iter()
            .find(|runtime| runtime.provider_id() == errand.selection.provider)
            .cloned()
        else {
            return Err(ErrandFailure::ProviderNotHosted);
        };
        if !self
            .settings
            .borrow()
            .settings
            .provider_enabled(&errand.selection.provider)
        {
            return Err(ErrandFailure::ProviderDisabled);
        }
        // A server already stopping never calls the Provider, so the Errand
        // never reaches one and leaves no record.
        if *self.shutdown.borrow() {
            return Err(ErrandFailure::Abandoned);
        }
        let reached = Reached {
            ledger: self.ledger.clone(),
            clock: self.clock.clone(),
            unrecorded: Some(Began {
                purpose,
                provider: errand.selection.provider.clone(),
                model: errand.selection.model.clone(),
                at: self.clock.now(),
            }),
        };
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => {
                reached.ended(ErrandOutcome::Abandoned, ErrandConsumption::default());
                Err(ErrandFailure::Abandoned)
            }
            answered = tokio::time::timeout(self.timeout, runtime.run_errand(errand)) => match answered {
                Ok(Ok(answer)) => {
                    reached.ended(ErrandOutcome::Answered, answer.consumption);
                    Ok(answer.reply)
                }
                Ok(Err(failed)) => {
                    reached.ended(ErrandOutcome::Failed, *failed.consumption);
                    Err(ErrandFailure::Provider(failed.error.to_string()))
                }
                Err(_) => {
                    reached.ended(ErrandOutcome::TimedOut, ErrandConsumption::default());
                    Err(ErrandFailure::TimedOut)
                }
            },
        }
    }
}

/// What is known of an Errand from the moment it reached its Provider.
struct Began {
    purpose: ErrandPurpose,
    provider: ProviderId,
    model: ModelId,
    at: SessionTimestamp,
}

/// An Errand that has reached its Provider and is still to be recorded. Let
/// go of unrecorded — its caller stopped waiting on it — it records the
/// Errand abandoned, so no Errand that reached a Provider goes unrecorded.
struct Reached {
    ledger: Arc<dyn ErrandLedger>,
    clock: ServerClock,
    unrecorded: Option<Began>,
}

impl Reached {
    fn ended(mut self, outcome: ErrandOutcome, consumption: ErrandConsumption) {
        self.record(outcome, consumption);
    }

    fn record(&mut self, outcome: ErrandOutcome, consumption: ErrandConsumption) {
        let Some(began) = self.unrecorded.take() else {
            return;
        };
        let record = ErrandRecord {
            purpose: began.purpose,
            provider: began.provider,
            model: began.model,
            began: began.at,
            ended: self.clock.now(),
            outcome,
            usage: consumption.usage,
            cost: consumption.cost,
        };
        // The answer is already the caller's whatever becomes of its record.
        if let Err(error) = self.ledger.record(record) {
            tracing::warn!("could not record an Errand: {error}");
        }
    }
}

impl Drop for Reached {
    fn drop(&mut self) {
        self.record(ErrandOutcome::Abandoned, ErrandConsumption::default());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{
        clock::ManualClock,
        protocol::{AgentSelection, Cost},
        provider::{
            ErrandAnswer, ErrandFailed, ErrandFuture, ProviderError, ProviderFuture,
            ProviderModelDiscovery, ProviderSessionConnection, ProviderSessionRequest,
        },
    };

    /// The Provider these tests route to. A real Provider id, because
    /// Enablement is only readable for a Provider the Settings schema names.
    const PROVIDER: &str = "codex";

    const MODEL: &str = "errand-model";

    /// How long each scripted Provider takes to answer, by the test's clock.
    const ANSWERING_TAKES: Duration = Duration::from_millis(2_400);

    /// How a scripted Provider meets an Errand.
    #[derive(Clone)]
    enum Script {
        Answer(ErrandAnswer),
        Fail(&'static str, ErrandConsumption),
        /// Answers nothing, so waiting on it ends some other way.
        Silence,
    }

    /// A Provider that counts every Errand it is asked to run, takes
    /// [`ANSWERING_TAKES`] by the test's clock to meet it, and meets it as
    /// its script says.
    struct ScriptedRuntime {
        script: Script,
        errands: Arc<AtomicUsize>,
        hand: ManualClock,
    }

    impl ProviderRuntime for ScriptedRuntime {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(PROVIDER)
        }

        fn display_name(&self) -> &str {
            "Scripted"
        }

        fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery> {
            Box::pin(async { Ok(ProviderModelDiscovery::new(Vec::new())) })
        }

        fn start_session(
            &self,
            _request: ProviderSessionRequest,
        ) -> ProviderFuture<'_, ProviderSessionConnection> {
            unimplemented!("Errand tests never start Sessions")
        }

        fn run_errand(&self, _errand: ProviderErrand) -> ErrandFuture<'_> {
            self.errands.fetch_add(1, Ordering::SeqCst);
            self.hand.advance(ANSWERING_TAKES);
            match self.script.clone() {
                Script::Answer(answer) => Box::pin(async move { Ok(answer) }),
                Script::Fail(message, consumption) => Box::pin(async move {
                    Err(ErrandFailed::measured(
                        ProviderError::new(message),
                        consumption,
                    ))
                }),
                Script::Silence => Box::pin(std::future::pending()),
            }
        }

        // These tests hand the runner a Selection outright, so nothing here
        // resolves a declaration.
        fn errand_selection(&self) -> Option<AgentSelection> {
            None
        }

        fn shutdown(&self) -> ProviderFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A ledger keeping every record it takes, or refusing every one.
    #[derive(Clone, Default)]
    struct Ledger {
        records: Arc<Mutex<Vec<ErrandRecord>>>,
        refuses: bool,
    }

    impl Ledger {
        fn records(&self) -> Vec<ErrandRecord> {
            self.records.lock().expect("ledger lock").clone()
        }
    }

    impl ErrandLedger for Ledger {
        fn record(&self, record: ErrandRecord) -> Result<(), RecordRefused> {
            if self.refuses {
                return Err(RecordRefused("the writer is no longer running".to_owned()));
            }
            self.records.lock().expect("ledger lock").push(record);
            Ok(())
        }
    }

    fn errand() -> ProviderErrand {
        ProviderErrand {
            prompt: "name this".to_owned(),
            schema: serde_json::json!({}),
            selection: AgentSelection {
                provider: ProviderId::new(PROVIDER),
                model: ModelId::new(MODEL),
                options: Vec::new(),
            },
            execution_directory: std::path::PathBuf::from("/workspace"),
        }
    }

    struct Harness {
        runner: ErrandRunner,
        asked: Arc<AtomicUsize>,
        ledger: Ledger,
        clock: ServerClock,
        shutdown: watch::Sender<bool>,
        _settings: watch::Sender<SettingsSnapshot>,
    }

    fn harness(script: Script) -> Harness {
        harness_with(script, SettingsSnapshot::default(), Ledger::default())
    }

    fn harness_with(script: Script, settings: SettingsSnapshot, ledger: Ledger) -> Harness {
        let (clock, hand) = ServerClock::manual();
        let asked = Arc::new(AtomicUsize::new(0));
        let runtime: Arc<dyn ProviderRuntime> = Arc::new(ScriptedRuntime {
            script,
            errands: asked.clone(),
            hand,
        });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (settings, settings_rx) = watch::channel(settings);
        Harness {
            // The timeout is long enough that a test reaching it has failed
            // rather than merely waited.
            runner: ErrandRunner::new(
                Arc::new(vec![runtime]),
                shutdown_rx,
                settings_rx,
                Arc::new(ledger.clone()),
            )
            .with_timeout(Duration::from_secs(30))
            .with_clock(clock.clone()),
            asked,
            ledger,
            clock,
            shutdown,
            _settings: settings,
        }
    }

    fn answered(usage: Option<Usage>, cost: Option<ErrandCost>) -> Script {
        Script::Answer(ErrandAnswer {
            reply: serde_json::json!({ "title": "Fix the flicker" }),
            consumption: ErrandConsumption { usage, cost },
        })
    }

    fn usage() -> Usage {
        Usage {
            fresh_input_tokens: Some(12),
            cache_read_tokens: Some(14_211),
            cache_write_tokens: Some(1_830),
            output_tokens: Some(56),
            reasoning_tokens: Some(40),
            native_meter: None,
        }
    }

    fn reported(usd: f64) -> ErrandCost {
        ErrandCost::reported(Cost::from_usd(usd).expect("a representable Cost"))
    }

    fn after(began: SessionTimestamp, took: Duration) -> SessionTimestamp {
        SessionTimestamp(began.0 + u64::try_from(took.as_millis()).expect("a short wait"))
    }

    #[tokio::test]
    async fn an_answered_errand_is_recorded_with_what_it_consumed() {
        let harness = harness(answered(Some(usage()), Some(reported(0.004_123_5))));
        let began = harness.clock.now();

        let reply = harness
            .runner
            .run(ErrandPurpose::SessionTitle, errand())
            .await
            .expect("the Provider answered");

        assert_eq!(reply, serde_json::json!({ "title": "Fix the flicker" }));
        assert_eq!(
            harness.ledger.records(),
            vec![ErrandRecord {
                purpose: ErrandPurpose::SessionTitle,
                provider: ProviderId::new(PROVIDER),
                model: ModelId::new(MODEL),
                began,
                ended: after(began, ANSWERING_TAKES),
                outcome: ErrandOutcome::Answered,
                usage: Some(usage()),
                cost: Some(reported(0.004_123_5)),
            }]
        );
    }

    /// A Provider that cannot say what an Errand consumed leaves its Usage
    /// and Cost unknown rather than zero.
    #[tokio::test]
    async fn an_answer_stating_no_usage_is_recorded_with_none() {
        let harness = harness(answered(None, None));

        harness
            .runner
            .run(ErrandPurpose::TitleProposal, errand())
            .await
            .expect("the Provider answered");

        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.purpose, ErrandPurpose::TitleProposal);
        assert_eq!(record.outcome, ErrandOutcome::Answered);
        assert_eq!((record.usage, record.cost), (None, None));
    }

    #[tokio::test]
    async fn a_failed_errand_is_recorded_failed() {
        let harness = harness(Script::Fail(
            "the CLI would not start",
            ErrandConsumption::default(),
        ));
        let began = harness.clock.now();

        let failure = harness
            .runner
            .run(ErrandPurpose::WorkspaceDescription, errand())
            .await
            .expect_err("the Provider failed");

        assert!(matches!(failure, ErrandFailure::Provider(_)), "{failure}");
        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.purpose, ErrandPurpose::WorkspaceDescription);
        assert_eq!(record.outcome, ErrandOutcome::Failed);
        assert_eq!(
            (record.began, record.ended),
            (began, after(began, ANSWERING_TAKES))
        );
        assert_eq!((record.usage, record.cost), (None, None));
    }

    /// A Provider that failed after reaching its Model may still say what the
    /// attempt consumed, and the record keeps it.
    #[tokio::test]
    async fn a_failed_errand_keeps_what_its_provider_says_it_consumed() {
        let harness = harness(Script::Fail(
            "the CLI answered outside the schema",
            ErrandConsumption {
                usage: Some(usage()),
                cost: Some(reported(0.002)),
            },
        ));

        harness
            .runner
            .run(ErrandPurpose::SessionTitle, errand())
            .await
            .expect_err("the Provider failed");

        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.outcome, ErrandOutcome::Failed);
        assert_eq!(
            (record.usage, record.cost),
            (Some(usage()), Some(reported(0.002)))
        );
    }

    /// Suru stopped listening before the Provider could say what the Errand
    /// consumed, so its Usage is unknown.
    #[tokio::test]
    async fn a_timed_out_errand_is_recorded_with_its_usage_unknown() {
        let mut harness = harness(Script::Silence);
        harness.runner = harness.runner.with_timeout(Duration::from_millis(5));

        let failure = harness
            .runner
            .run(ErrandPurpose::SessionTitle, errand())
            .await
            .expect_err("the Provider never answered");

        assert!(matches!(failure, ErrandFailure::TimedOut), "{failure}");
        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.outcome, ErrandOutcome::TimedOut);
        assert_eq!((record.usage, record.cost), (None, None));
    }

    /// A shutting-down server abandons an Errand outright rather than waiting
    /// on a Provider that is no longer going to answer, and records it
    /// abandoned with its Usage unknown.
    #[tokio::test]
    async fn a_shutdown_abandons_an_errand_still_waiting_on_its_provider() {
        let harness = harness(Script::Silence);
        let runner = harness.runner.clone();

        let running =
            tokio::spawn(async move { runner.run(ErrandPurpose::SessionTitle, errand()).await });
        // The Errand reaches the Provider before the server stops, so what the
        // shutdown interrupts is a call already in flight.
        while harness.asked.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        harness.shutdown.send_replace(true);

        assert!(matches!(
            running.await.expect("the Errand task completes"),
            Err(ErrandFailure::Abandoned)
        ));
        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.outcome, ErrandOutcome::Abandoned);
        assert_eq!((record.usage, record.cost), (None, None));
    }

    /// A server already stopping never calls the Provider, so the Errand never
    /// reaches one and nothing is recorded.
    #[tokio::test]
    async fn an_errand_asked_for_once_shutdown_began_records_nothing() {
        let harness = harness(answered(Some(usage()), None));
        harness.shutdown.send_replace(true);

        assert!(matches!(
            harness
                .runner
                .run(ErrandPurpose::SessionTitle, errand())
                .await,
            Err(ErrandFailure::Abandoned)
        ));
        assert_eq!(harness.asked.load(Ordering::SeqCst), 0);
        assert_eq!(harness.ledger.records(), Vec::new());
    }

    /// A caller that stops waiting — a Title proposal whose asker went away —
    /// abandons the Errand with it, and the Errand is still recorded.
    #[tokio::test]
    async fn an_errand_its_caller_stops_waiting_on_is_recorded_abandoned() {
        let harness = harness(Script::Silence);
        let runner = harness.runner.clone();

        let running =
            tokio::spawn(async move { runner.run(ErrandPurpose::TitleProposal, errand()).await });
        while harness.asked.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        running.abort();
        let _ = running.await;

        let [record] = harness.ledger.records().try_into().expect("one record");
        assert_eq!(record.purpose, ErrandPurpose::TitleProposal);
        assert_eq!(record.outcome, ErrandOutcome::Abandoned);
        assert_eq!((record.usage, record.cost), (None, None));
    }

    /// Enablement is the user's own choice, and a disabled Provider is one Suru
    /// leaves entirely alone — so no Errand is made of it, Suru does not walk
    /// to another Provider in its place, and there is nothing to record.
    #[tokio::test]
    async fn a_disabled_provider_is_never_asked_to_run_an_errand() {
        let mut snapshot = SettingsSnapshot::default();
        snapshot.settings.provider.codex.enabled = false;
        let harness = harness_with(answered(Some(usage()), None), snapshot, Ledger::default());

        assert!(matches!(
            harness
                .runner
                .run(ErrandPurpose::SessionTitle, errand())
                .await,
            Err(ErrandFailure::ProviderDisabled)
        ));
        assert_eq!(
            harness.asked.load(Ordering::SeqCst),
            0,
            "a Provider the user turned off is not called at all"
        );
        assert_eq!(harness.ledger.records(), Vec::new());
    }

    #[tokio::test]
    async fn an_errand_for_a_provider_not_hosted_records_nothing() {
        let harness = harness(answered(Some(usage()), None));
        let mut elsewhere = errand();
        elsewhere.selection.provider = ProviderId::new("claude");

        assert!(matches!(
            harness
                .runner
                .run(ErrandPurpose::SessionTitle, elsewhere)
                .await,
            Err(ErrandFailure::ProviderNotHosted)
        ));
        assert_eq!(harness.ledger.records(), Vec::new());
    }

    /// Recording is no part of answering: a record storage refuses is
    /// logged, and the answer stands.
    #[tokio::test]
    async fn a_record_refused_leaves_the_answer_unchanged() {
        let harness = harness_with(
            answered(Some(usage()), Some(reported(0.01))),
            SettingsSnapshot::default(),
            Ledger {
                refuses: true,
                ..Ledger::default()
            },
        );

        assert_eq!(
            harness
                .runner
                .run(ErrandPurpose::SessionTitle, errand())
                .await
                .expect("the Provider answered"),
            serde_json::json!({ "title": "Fix the flicker" })
        );
    }
}
