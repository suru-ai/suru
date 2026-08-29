//! Running Suru's own Provider work as Errands.
//!
//! An Errand is one Provider call Suru makes for its own purposes rather than
//! the user's: one Prompt in, one reply shaped by a schema, carrying no Tools
//! and belonging to no Session. This module owns everything true of *every*
//! Errand — which runtime runs it, how long it may take, and that a server
//! shutting down abandons it — and nothing about what any particular Errand is
//! for. Deriving a Session's Title is the only caller today; compaction and
//! summarization are meant to arrive here unchanged (ADR 0011).

use std::sync::Arc;

use serde_json::Value;
use tokio::{sync::watch, time::Duration};

use crate::{
    protocol::SettingsSnapshot,
    provider::{ProviderErrand, ProviderRuntime, wait_for_shutdown},
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
}

impl ErrandRunner {
    pub(crate) fn new(
        runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
        shutdown: watch::Receiver<bool>,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        Self {
            runtimes,
            timeout: DEFAULT_ERRAND_TIMEOUT,
            shutdown,
            settings,
        }
    }

    /// Bounds how long an Errand may take; injectable so tests exercise the
    /// deadline without waiting out the default.
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs one Errand and answers with the JSON the Provider replied. The
    /// reply is unvalidated: the schema is a request rather than a guarantee,
    /// so the caller checks the shape itself.
    pub(crate) async fn run(&self, errand: ProviderErrand) -> Result<Value, ErrandFailure> {
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
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => Err(ErrandFailure::Abandoned),
            answered = tokio::time::timeout(self.timeout, runtime.run_errand(errand)) => match answered {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(ErrandFailure::Provider(error.to_string())),
                Err(_) => Err(ErrandFailure::TimedOut),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        protocol::{AgentSelection, ModelId, ProviderId},
        provider::{
            ProviderFuture, ProviderModelDiscovery, ProviderSessionConnection,
            ProviderSessionRequest,
        },
    };

    /// The Provider these tests route to. A real Provider id, because
    /// Enablement is only readable for a Provider the Settings schema names.
    const PROVIDER: &str = "codex";

    /// A Provider that counts every Errand it is asked to run and answers none
    /// of them, so a test can see both that it was asked and that waiting on it
    /// ended some other way.
    struct SilentRuntime {
        errands: Arc<AtomicUsize>,
    }

    impl ProviderRuntime for SilentRuntime {
        fn provider_id(&self) -> ProviderId {
            ProviderId::new(PROVIDER)
        }

        fn display_name(&self) -> &str {
            "Silent"
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

        fn run_errand(&self, _errand: ProviderErrand) -> ProviderFuture<'_, Value> {
            self.errands.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
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

    fn errand() -> ProviderErrand {
        ProviderErrand {
            prompt: "name this".to_owned(),
            schema: serde_json::json!({}),
            selection: AgentSelection {
                provider: ProviderId::new(PROVIDER),
                model: ModelId::new("silent-model"),
                options: Vec::new(),
            },
            workspace: std::path::PathBuf::from("/workspace"),
        }
    }

    fn runner(
        shutdown: watch::Receiver<bool>,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> (ErrandRunner, Arc<AtomicUsize>) {
        let errands = Arc::new(AtomicUsize::new(0));
        let runtime: Arc<dyn ProviderRuntime> = Arc::new(SilentRuntime {
            errands: errands.clone(),
        });
        (
            // The timeout is long enough that a test reaching it has failed
            // rather than merely waited.
            ErrandRunner::new(Arc::new(vec![runtime]), shutdown, settings)
                .with_timeout(Duration::from_secs(30)),
            errands,
        )
    }

    /// A shutting-down server abandons an Errand outright rather than waiting
    /// on a Provider that is no longer going to answer. Exercised here because
    /// abandonment deliberately leaves nothing behind for a server-seam test to
    /// observe.
    #[tokio::test]
    async fn a_shutdown_abandons_an_errand_still_waiting_on_its_provider() {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (_settings, settings_rx) = watch::channel(SettingsSnapshot::default());
        let (runner, asked) = runner(shutdown_rx, settings_rx);

        let running = tokio::spawn(async move { runner.run(errand()).await });
        // The Errand reaches the Provider before the server stops, so what the
        // shutdown interrupts is a call already in flight.
        while asked.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        shutdown.send_replace(true);

        assert!(matches!(
            running.await.expect("the Errand task completes"),
            Err(ErrandFailure::Abandoned)
        ));
    }

    /// Enablement is the user's own choice, and a disabled Provider is one Suru
    /// leaves entirely alone — so no Errand is made of it, and Suru does not
    /// walk to another Provider in its place.
    #[tokio::test]
    async fn a_disabled_provider_is_never_asked_to_run_an_errand() {
        let mut snapshot = SettingsSnapshot::default();
        snapshot.settings.provider.codex.enabled = false;
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let (_settings, settings_rx) = watch::channel(snapshot);
        let (runner, asked) = runner(shutdown_rx, settings_rx);

        assert!(matches!(
            runner.run(errand()).await,
            Err(ErrandFailure::ProviderDisabled)
        ));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            0,
            "a Provider the user turned off is not called at all"
        );
    }
}
