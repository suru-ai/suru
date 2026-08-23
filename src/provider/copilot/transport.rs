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
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
};

use github_copilot_sdk::{Client, rpc::Model};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    process::ChildStdin,
};

use super::{copilot_error, copilot_error_context};
use crate::provider::{
    ProviderError, ProviderFuture,
    harness::{HarnessConnector, HarnessLink, ProcessStdio},
};

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

    fn connect(&self, io: ProcessStdio) -> Result<CopilotConnection, ProviderError> {
        let stdin = HarnessStdin::new(io.stdin);
        let client = Client::from_streams(io.stdout, stdin.clone(), self.cwd.clone())
            .map_err(|error| copilot_error(format!("could not drive the Copilot CLI: {error}")))?;
        Ok(CopilotConnection {
            client,
            stdin,
            ending: Arc::new(StdMutex::new(None)),
        })
    }

    fn initialize(&self, connection: &CopilotConnection) -> ProviderFuture<'_, ()> {
        let connection = connection.clone();
        Box::pin(async move {
            connection
                .client
                .verify_protocol_version()
                .await
                .map_err(|error| connection.failure("Copilot CLI server handshake failed", error))
        })
    }
}

/// The live connection to one Copilot CLI server process: the SDK client every demand speaks
/// through, and the supervisor's grip on the same process.
#[derive(Clone)]
pub(super) struct CopilotConnection {
    client: Client,
    stdin: HarnessStdin,
    ending: Arc<StdMutex<Option<ConnectionEnd>>>,
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
    /// The Models Copilot offers the signed-in user right now.
    ///
    /// The SDK's own `list_models` memoizes for the life of its client, and this connection outlives
    /// every catalog refresh, so this asks the CLI each time.
    pub(super) async fn list_models(&self) -> Result<Vec<Model>, ProviderError> {
        self.client
            .rpc()
            .models()
            .list()
            .await
            .map(|listed| listed.models)
            .map_err(|error| self.failure("Copilot Model discovery failed", error))
    }

    /// The Provider failure to report for `error` under `context`.
    ///
    /// Once the process is gone every in-flight and later request fails with a transport-shaped SDK
    /// error that says nothing about why. The supervisor already decided what the exit meant — and
    /// whether it lost a Provider Session — so its account wins wherever it exists.
    fn failure(&self, context: &str, error: impl std::fmt::Display) -> ProviderError {
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

    /// Records how the connection ended, keeping the first account: a process that exits on its own
    /// while a shutdown is already under way did not lose anything the shutdown was not taking.
    fn record(&self, ending: ConnectionEnd) {
        self.ending
            .lock()
            .expect("Copilot connection ending lock is not poisoned")
            .get_or_insert(ending);
    }
}

impl HarnessLink for CopilotConnection {
    fn close(&self) {
        self.record(ConnectionEnd::Closed);
    }

    fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.stdin.close())
    }

    fn terminate(&self, error: ProviderError) {
        self.record(ConnectionEnd::Terminated(error));
    }
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
