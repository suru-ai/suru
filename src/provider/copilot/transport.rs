//! Suru's client over one Copilot CLI server process.
//!
//! The SDK's [`Client`] owns the transport once it has the process's stdio, so this module is what
//! keeps the harness supervisor in charge of it: [`CopilotConnection`] hands the supervisor a
//! [`HarnessLink`] over the same process, and remembers how the connection ended so a request that
//! fails on a dead pipe reports the supervisor's account of the exit rather than an I/O error.

use std::{
    future::Future,
    io,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    task::{Context, Poll},
};

use github_copilot_sdk::{Client, ErrorKind, ProtocolErrorKind, rpc::Model};
use serde::Deserialize;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    process::ChildStdin,
};

use super::{
    concise_remote_message, copilot_error, copilot_error_context,
    event_drain::{CopilotEventDrain, EventDrainCheckpoint},
    pricing::CopilotPricing,
};
use crate::{
    protocol::ProviderUnavailability,
    provider::{
        ProviderError, ProviderFuture,
        harness::{HarnessConnector, HarnessInvalidator, HarnessLink, ProcessStdio},
        version::SuggestedCliVersion,
    },
};

const COPILOT_SUGGESTED_VERSION: SuggestedCliVersion =
    SuggestedCliVersion::new("Copilot CLI", 1, 0, 88);

/// Builds the Copilot client over each freshly launched harness process.
pub(super) struct CopilotConnector {
    /// The directory the SDK client reports as its own. Copilot resolves a Session's Workspace from
    /// the Session's own configuration, so this only ever backs a request that names none.
    cwd: PathBuf,
}

impl CopilotConnector {
    pub(super) fn new() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

impl HarnessConnector for CopilotConnector {
    type Connection = CopilotConnection;

    fn connect(
        &self,
        io: ProcessStdio,
        invalidator: HarnessInvalidator,
    ) -> Result<CopilotConnection, ProviderError> {
        let stdin = HarnessStdin::new(io.stdin);
        let event_drain = CopilotEventDrain::new();
        let stdout = event_drain.observe(io.stdout);
        let client = Client::from_streams(stdout, stdin.clone(), self.cwd.clone())
            .map_err(|error| copilot_error(format!("could not drive the Copilot CLI: {error}")))?;
        Ok(CopilotConnection {
            client,
            stdin,
            ending: Arc::new(StdMutex::new(None)),
            invalidator,
            event_drain,
            warning: Arc::new(OnceLock::new()),
            pricing: CopilotPricing::default(),
        })
    }

    fn initialize(&self, connection: &CopilotConnection) -> ProviderFuture<'_, ()> {
        let connection = connection.clone();
        Box::pin(async move {
            connection
                .client
                .verify_protocol_version()
                .await
                .map_err(|error| {
                    // The version the CLI reported is the SDK's own answer rather than anything
                    // the process's exit could explain, so drift is reported from the handshake
                    // itself while every other way it can fail defers to the supervisor.
                    if is_version_drift(&error) {
                        return version_drift(&error);
                    }
                    connection.failure("Copilot CLI server handshake failed", error)
                })?;
            // `status.get` is advisory rather than part of the protocol handshake. A CLI old
            // enough not to answer it remains governed by the SDK's hard protocol check above;
            // Suru simply has no package-version guidance to add.
            let warning = connection
                .client
                .get_status()
                .await
                .ok()
                .and_then(|status| {
                    COPILOT_SUGGESTED_VERSION
                        .warning_for(&status.version)
                        .ok()
                        .flatten()
                });
            let _ = connection.warning.set(warning);
            Ok(())
        })
    }
}

/// JSON-RPC's own code for a method the server does not have. The SDK keeps its copy private, so
/// Suru names the wire constant itself.
const METHOD_NOT_FOUND: i32 = -32601;

/// Whether `error` is the handshake refusing the CLI's protocol version rather than the transport
/// failing under it. Every shape the SDK reports drift in counts: a version outside the range it
/// speaks, one it cannot even read, and one that changed under a live connection.
fn is_version_drift(error: &github_copilot_sdk::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Protocol(
            ProtocolErrorKind::VersionMismatch { .. }
                | ProtocolErrorKind::InvalidProtocolVersion { .. }
                | ProtocolErrorKind::VersionChanged { .. }
        )
    )
}

/// A CLI too old or too new for the protocol Suru speaks, as the typed condition. The user owns
/// their own CLI, so version drift is theirs to fix: reported this way it reads as an actionable
/// condition rather than as a Provider that broke.
fn version_drift(error: &impl std::fmt::Display) -> ProviderError {
    copilot_error(format!(
        "Copilot CLI server speaks another protocol: {error}"
    ))
    .mark_unavailable(ProviderUnavailability::IncompatibleVersion)
}

/// The live connection to one Copilot CLI server process: the SDK client every demand speaks
/// through, and the supervisor's grip on the same process.
#[derive(Clone)]
pub(super) struct CopilotConnection {
    client: Client,
    stdin: HarnessStdin,
    ending: Arc<StdMutex<Option<ConnectionEnd>>>,
    invalidator: HarnessInvalidator,
    event_drain: CopilotEventDrain,
    /// Non-blocking package-version guidance learned once when this process handshakes.
    warning: Arc<OnceLock<Option<String>>>,
    /// The prices most recently published by this harness generation. Session
    /// projections share it so a catalog refresh changes only future recorded
    /// Costs; Turns already stamped by the store remain frozen.
    pricing: CopilotPricing,
}

/// Why this connection stopped serving requests, once it has.
#[derive(Clone)]
enum ConnectionEnd {
    /// The supervisor asked the process to stop; nothing was lost that was not already going.
    Closed,
    /// The process exited on its own, taking everything it hosted with it.
    Terminated(ProviderError),
}

impl CopilotConnection {
    pub(super) fn compatibility_warning(&self) -> Option<String> {
        self.warning.get().cloned().flatten()
    }

    /// The Models Copilot offers the signed-in user right now.
    ///
    /// The Models are that user's, so a discovery asks the CLI about its credentials before it asks
    /// for a catalog: a CLI holding none reports the typed not-signed-in condition rather than
    /// whatever it answers a catalog request with.
    ///
    /// The SDK's own `list_models` memoizes for the life of its client, and this connection outlives
    /// every catalog refresh, so this asks the CLI each time.
    pub(super) async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        self.verify_signed_in().await?;
        let models = self
            .client
            .rpc()
            .models()
            .list()
            .await
            .map(|listed| listed.models)
            .map_err(|error| self.failure("Copilot Model discovery failed", error))?;
        self.pricing.replace(&models);
        Ok(models)
    }

    pub(super) fn pricing(&self) -> CopilotPricing {
        self.pricing.clone()
    }

    /// Loads Copilot's published prices once for a harness generation. A
    /// Session can already have a Model in force without the client ever
    /// listing Models, so startup asks here independently; failure deliberately
    /// degrades only Cost and leaves the Session usable.
    pub(super) async fn ensure_pricing(&self) {
        if self.pricing.is_loaded() {
            return;
        }
        let _ = self.list_models().await;
    }

    /// Fails with the typed not-signed-in condition unless the CLI holds credentials it can work
    /// with right now.
    ///
    /// Authentication is entirely between the user and the Copilot CLI — its own login, `gh`
    /// credentials, or an environment token — so Suru neither asks for nor stores any of it. All a
    /// discovery can do about a signed-out CLI is say so, and ask again on the next refresh.
    ///
    /// The query goes over the wire directly rather than through the SDK's typed method, whose
    /// reading of the answer insists on a token the CLI withholds; see [`CurrentAuth`].
    async fn verify_signed_in(&self) -> Result<(), ProviderError> {
        let answer = self
            .client
            .call(CURRENT_AUTH_METHOD, Some(serde_json::json!({})))
            .await
            .map_err(|error| {
                // A CLI that has never heard of the sign-in query is one Suru cannot ask, which is
                // version drift the handshake could not catch: the SDK falls back to a legacy ping
                // for a CLI too old to answer `connect`, so the first unknown method is where such
                // a CLI gives itself away.
                if error.rpc_code() == Some(METHOD_NOT_FOUND) {
                    return version_drift(&error);
                }
                self.failure("Copilot sign-in check failed", error)
            })?;
        let auth: CurrentAuth = serde_json::from_value(answer)
            .map_err(|error| copilot_error(format!("Copilot sign-in check failed: {error}")))?;
        if auth.auth_info.is_some() {
            return Ok(());
        }
        Err(ProviderError::unavailable(
            ProviderUnavailability::NotSignedIn,
            signed_out_message(auth.auth_errors.unwrap_or_default()),
        ))
    }

    /// The SDK client every request on this connection goes through.
    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    /// The Provider failure to report for `error` under `context`.
    ///
    /// Once the process is gone every in-flight and later request fails with a transport-shaped SDK
    /// error that says nothing about why. The supervisor's account wins when it has arrived; when
    /// the SDK observes the dead transport first, that typed signal invalidates this shared-harness
    /// generation so a later demand cannot be granted the same connection.
    pub(super) fn failure(&self, context: &str, error: github_copilot_sdk::Error) -> ProviderError {
        if error.is_transport_failure() {
            let lost = copilot_error(format!(
                "{} connection was lost: {error}",
                super::COPILOT_HARNESS_NAME
            ))
            .mark_session_lost();
            if let ConnectionEnd::Terminated(lost) = self.record(ConnectionEnd::Terminated(lost)) {
                self.invalidator.invalidate(lost);
            }
        }
        match self.end() {
            Some(ConnectionEnd::Terminated(exit)) => copilot_error_context(context, exit),
            Some(ConnectionEnd::Closed) => copilot_error(format!(
                "{context}: {} is shutting down",
                super::COPILOT_HARNESS_NAME
            )),
            None => copilot_error(format!("{context}: {error}")),
        }
    }

    fn end(&self) -> Option<ConnectionEnd> {
        self.ending
            .lock()
            .expect("Copilot connection ending lock is not poisoned")
            .clone()
    }

    /// The on-wire position a Session begins observing from, used to drain its final events before
    /// a process crash settles it.
    pub(super) fn event_checkpoint(&self, session_id: &str) -> EventDrainCheckpoint {
        self.event_drain.checkpoint(session_id)
    }

    /// Records how the connection ended, keeping the first account: a process that exits on its own
    /// while a shutdown is already under way did not lose anything the shutdown was not taking.
    fn record(&self, ending: ConnectionEnd) -> ConnectionEnd {
        self.ending
            .lock()
            .expect("Copilot connection ending lock is not poisoned")
            .get_or_insert(ending)
            .clone()
    }
}

impl HarnessLink for CopilotConnection {
    fn close(&self) {
        let _ = self.record(ConnectionEnd::Closed);
    }

    fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.stdin.close())
    }

    fn terminate(&self, error: ProviderError) {
        let _ = self.record(ConnectionEnd::Terminated(error));
    }
}

/// The wire method behind the SDK's `account.getCurrentAuth`, which Suru asks for itself.
const CURRENT_AUTH_METHOD: &str = "account.getCurrentAuth";

/// The CLI's answer to the sign-in query, read only as far as Suru needs it.
///
/// The SDK (1.0.15-preview.3) types this answer as its credential-bearing `AuthInfo`, an
/// untagged union whose `gh-cli`, `env`, and `token` shapes each require a `token`, while the
/// CLI (1.0.88 onward) answers with credential-free metadata that carries none; the SDK's own
/// schema notes as much. So a CLI signed in through the gh CLI or an environment token answers in
/// a shape the SDK cannot read, and its typed query fails a signed-in CLI as unreadable
/// (docs/validation/copilot-signin-credential-shape.md). Suru wants only whether credentials are
/// on file and what the CLI said when it last found none, so it reads exactly that and lets the
/// credentials stay the CLI's own business, as ADR-0008 wants them to.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentAuth {
    /// Whatever the CLI says about the credentials it holds; present exactly when it holds some.
    #[serde(default)]
    auth_info: Option<serde_json::Value>,
    /// What went wrong the last time the CLI tried to resolve credentials, if anything did.
    #[serde(default)]
    auth_errors: Option<Vec<String>>,
}

/// What a user reads about a CLI holding no usable credentials: what to do about it, and whatever
/// the CLI said went wrong last time it tried, which is the only account of why there are none.
fn signed_out_message(auth_errors: Vec<String>) -> String {
    let mut message = "sign in with the Copilot CLI".to_owned();
    if !auth_errors.is_empty() {
        message.push_str(&format!(": {}", auth_errors.join("; ")));
    }
    concise_remote_message(&message, super::COPILOT_FAILURE_FALLBACK)
}

/// The harness process's stdin, writable by the SDK client and closable by the supervisor.
///
/// The SDK takes ownership of whatever it writes through, so the supervisor keeps its own grip on
/// the pipe here. Only the SDK's single writer task writes, and closing is a one-off, so the lock is
/// never held across an await.
#[derive(Clone)]
struct HarnessStdin(Arc<StdMutex<Option<ChildStdin>>>);

impl HarnessStdin {
    fn new(stdin: ChildStdin) -> Self {
        Self(Arc::new(StdMutex::new(Some(stdin))))
    }

    /// Closes the harness's stdin so the process can exit on its own.
    async fn close(&self) {
        let stdin = self
            .0
            .lock()
            .expect("Copilot harness stdin lock is not poisoned")
            .take();
        if let Some(mut stdin) = stdin {
            let _ = stdin.shutdown().await;
        }
    }

    fn with_stdin<T>(
        &self,
        act: impl FnOnce(Pin<&mut ChildStdin>) -> Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        let mut stdin = self
            .0
            .lock()
            .expect("Copilot harness stdin lock is not poisoned");
        match stdin.as_mut() {
            Some(stdin) => act(Pin::new(stdin)),
            None => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
        }
    }
}

impl AsyncWrite for HarnessStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.with_stdin(|stdin| stdin.poll_write(context, buffer))
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.with_stdin(|stdin| stdin.poll_flush(context))
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.with_stdin(|stdin| stdin.poll_shutdown(context))
    }
}
