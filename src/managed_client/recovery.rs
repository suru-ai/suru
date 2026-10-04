//! Managed connection establishment, replacement, and crash recovery orchestration.

use std::{collections::HashSet, time::Duration};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, watch};

use crate::protocol::{
    Health, LifecycleState, PROTOCOL_VERSION, RuntimeDescriptor, ServerShutdown, ShutdownReason,
};

use super::{
    ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus,
    event_stream::{self, StreamOutcome},
    launcher,
    lifecycle::{self, Registration},
    session_catalog_stream,
};

/// One exponential retry schedule shared by managed, Remote catalog, and
/// Session recovery so all three honor the same configured policy.
pub(crate) struct RecoveryBackoff {
    current: Duration,
    initial: Duration,
    max: Duration,
}

impl RecoveryBackoff {
    pub(crate) fn new(initial: Duration, max: Duration) -> Self {
        Self {
            current: Duration::ZERO,
            initial,
            max,
        }
    }

    pub(crate) fn next(&mut self) -> Duration {
        self.current = if self.current.is_zero() {
            self.initial
        } else {
            self.current.saturating_mul(2).min(self.max)
        };
        self.current
    }

    pub(crate) fn reset(&mut self) {
        self.current = Duration::ZERO;
    }
}

struct ActiveConnection {
    descriptor: RuntimeDescriptor,
    health: Health,
    lifecycle_response: reqwest::Response,
    catalog_response: reqwest::Response,
}

struct ManagedStreamResponses {
    lifecycle: reqwest::Response,
    catalog: reqwest::Response,
}

pub(super) async fn connect(config: ManagedClientConfig) -> Result<ManagedClient> {
    let baseline = launcher::StopBaseline::read(&config);
    let deadline = tokio::time::Instant::now() + config.startup_timeout;
    let http = reqwest::Client::new();
    let connection = establish_connection(&config, &http, deadline, baseline).await?;
    let (events_tx, events_rx) = mpsc::channel(32);
    let (descriptor_tx, descriptor_rx) = watch::channel(connection.descriptor.clone());
    let managed_http = http.clone();
    let initial_recovery_backoff = config.initial_recovery_backoff;
    let max_recovery_backoff = config.max_recovery_backoff;
    let attachment_fetch_timeout = config.attachment_fetch_timeout;
    let config_dir = config.runtime.config_dir().map(ToOwned::to_owned);
    let task = tokio::spawn(run_managed_client(
        config,
        managed_http,
        connection,
        baseline,
        events_tx,
        descriptor_tx,
    ));
    Ok(ManagedClient {
        events: events_rx,
        http,
        descriptor: descriptor_rx,
        initial_recovery_backoff,
        max_recovery_backoff,
        attachment_fetch_timeout,
        config_dir,
        task,
    })
}

async fn establish_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    deadline: tokio::time::Instant,
    baseline: launcher::StopBaseline,
) -> Result<ActiveConnection> {
    let Registration { descriptor, health } =
        launcher::ensure_server(config, deadline, baseline).await?;
    // The server found ready may be stopped manually before its streams
    // open, and then that stop is why they did not.
    let streams =
        match open_managed_streams(http, &descriptor, deadline, config.startup_timeout).await {
            Ok(streams) => streams,
            Err(error) => {
                baseline.ensure_not_stopped(config)?;
                return Err(launcher::startup_error(config, &error.to_string()));
            }
        };
    Ok(ActiveConnection {
        descriptor,
        health,
        lifecycle_response: streams.lifecycle,
        catalog_response: streams.catalog,
    })
}

async fn open_managed_streams(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    deadline: tokio::time::Instant,
    startup_timeout: Duration,
) -> Result<ManagedStreamResponses> {
    // Either failure drops the sibling future, including any response it has
    // already opened. Both handshakes spend the same remaining startup budget.
    let (lifecycle, catalog) = tokio::try_join!(
        open_required_stream(
            deadline,
            startup_timeout,
            "event stream",
            event_stream::open(http, descriptor),
        ),
        open_required_stream(
            deadline,
            startup_timeout,
            "Session catalog stream",
            session_catalog_stream::open(http, descriptor),
        ),
    )?;
    Ok(ManagedStreamResponses { lifecycle, catalog })
}

async fn open_required_stream(
    deadline: tokio::time::Instant,
    startup_timeout: Duration,
    name: &str,
    opening: impl std::future::Future<Output = reqwest::Result<reqwest::Response>>,
) -> Result<reqwest::Response> {
    match tokio::time::timeout_at(deadline, opening).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) if error.status().is_some() => {
            Err(anyhow!("server rejected the initial {name}: {error}"))
        }
        Ok(Err(error)) => Err(anyhow!("could not open the initial {name}: {error}")),
        Err(_) => Err(anyhow!(
            "initial {name} did not open within {startup_timeout:?}"
        )),
    }
}

async fn run_managed_client(
    config: ManagedClientConfig,
    http: reqwest::Client,
    mut connection: ActiveConnection,
    baseline: launcher::StopBaseline,
    events: mpsc::Sender<ManagedEvent>,
    descriptor: watch::Sender<RuntimeDescriptor>,
) {
    let mut known_session_ids: Option<HashSet<_>> = None;
    if events.send(ManagedEvent::Connecting).await.is_err() {
        return;
    }
    loop {
        let first_catalog_snapshot = known_session_ids.is_none();
        descriptor.send_replace(connection.descriptor.clone());
        let active_descriptor = connection.descriptor.clone();
        let active_instance_id = connection.descriptor.instance_id;
        let active_build_identity = connection.health.build_identity.clone();
        let (catalog_hydrated, catalog_hydration) = tokio::sync::oneshot::channel();
        let catalog = session_catalog_stream::consume(
            connection.catalog_response,
            &events,
            &mut known_session_ids,
            catalog_hydrated,
            false,
        );
        tokio::pin!(catalog);
        let mut initial_checkout_states = Vec::new();
        let stream_result = tokio::select! {
            biased;
            outcome = &mut catalog => Some(ActiveStreamResult::Catalog(outcome)),
            hydrated = catalog_hydration => {
                let Ok(checkout_states) = hydrated else { return };
                if first_catalog_snapshot {
                    initial_checkout_states = checkout_states;
                }
                None
            }
        };
        let stream_result = match stream_result {
            Some(stream_result) => stream_result,
            None => {
                tracing::info!(
                    instance_id = %connection.health.instance_id,
                    pid = connection.health.pid,
                    "connected to Suru server"
                );
                if events
                    .send(ManagedEvent::Connected(connection.health))
                    .await
                    .is_err()
                {
                    return;
                }
                for checkout_state in initial_checkout_states {
                    let changed = crate::protocol::CheckoutStateChanged {
                        checkout_id: checkout_state.association.id.clone(),
                        checkout_state: Some(checkout_state),
                    };
                    if events
                        .send(ManagedEvent::CheckoutStateChanged(changed))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let lifecycle = event_stream::consume(
                    connection.lifecycle_response,
                    &events,
                    active_instance_id,
                );
                tokio::pin!(lifecycle);
                tokio::select! {
                    biased;
                    outcome = &mut lifecycle => ActiveStreamResult::Lifecycle(outcome),
                    outcome = &mut catalog => {
                        if matches!(
                            outcome,
                            Ok(session_catalog_stream::StreamOutcome::Disconnected)
                        ) {
                            let health = lifecycle::inspect_descriptor_health(&active_descriptor);
                            tokio::pin!(health);
                            tokio::select! {
                                biased;
                                lifecycle = &mut lifecycle => {
                                    ActiveStreamResult::Lifecycle(lifecycle)
                                }
                                health = &mut health => match health {
                                    Ok(health) if health.lifecycle == LifecycleState::Ready => {
                                        ActiveStreamResult::Catalog(outcome)
                                    }
                                    Ok(_) | Err(_) => {
                                        ActiveStreamResult::Lifecycle(lifecycle.await)
                                    }
                                }
                            }
                        } else {
                            ActiveStreamResult::Catalog(outcome)
                        }
                    },
                }
            }
        };
        let replaced_instance_id = match stream_result {
            ActiveStreamResult::Lifecycle(Ok(StreamOutcome::Disconnected)) => None,
            ActiveStreamResult::Lifecycle(Ok(StreamOutcome::ManualShutdown)) => return,
            ActiveStreamResult::Lifecycle(Ok(StreamOutcome::Replacement { instance_id })) => {
                Some(instance_id)
            }
            ActiveStreamResult::Lifecycle(Ok(StreamOutcome::ReceiverClosed))
            | ActiveStreamResult::Catalog(Ok(
                session_catalog_stream::StreamOutcome::ReceiverClosed,
            )) => return,
            ActiveStreamResult::Catalog(Ok(
                session_catalog_stream::StreamOutcome::Disconnected,
            )) => None,
            ActiveStreamResult::Lifecycle(Err(error)) | ActiveStreamResult::Catalog(Err(error)) => {
                tracing::error!("managed server stream failed: {error:#}");
                let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                return;
            }
        };

        if let Some(replaced_instance_id) = replaced_instance_id {
            tracing::info!(%replaced_instance_id, "managed server is being replaced");
            if events
                .send(ManagedEvent::Recovering(RecoveryStatus {
                    attempt: 1,
                    retry_in: Duration::ZERO,
                }))
                .await
                .is_err()
            {
                return;
            }
            let deadline = tokio::time::Instant::now() + config.startup_timeout;
            match wait_for_protocol_compatible_connection(
                &config,
                &http,
                Some(replaced_instance_id),
                deadline,
                baseline,
            )
            .await
            {
                ConnectionWait::Ready(replacement) => {
                    connection = *replacement;
                    continue;
                }
                ConnectionWait::Stopped(stopped) => {
                    leave_on_manual_stop(&events, stopped).await;
                    return;
                }
                ConnectionWait::Incompatible(protocol_version) => {
                    let message = format!(
                        "replacement server protocol version {protocol_version} is incompatible with client protocol version {PROTOCOL_VERSION}"
                    );
                    tracing::error!("{message}");
                    let _ = events.send(ManagedEvent::Fatal(message)).await;
                    return;
                }
                ConnectionWait::TimedOut => {
                    let message = format!(
                        "replacement Suru server did not become ready within {:?}",
                        config.startup_timeout
                    );
                    tracing::error!("{message}");
                    let _ = events.send(ManagedEvent::Fatal(message)).await;
                    return;
                }
            }
        }

        let configured_build_can_restore =
            crate::build_identity::for_executable(&config.server_executable)
                .is_ok_and(|identity| identity == active_build_identity);
        let mut attempt = 1;
        let mut retry_in = Duration::ZERO;
        let mut backoff =
            RecoveryBackoff::new(config.initial_recovery_backoff, config.max_recovery_backoff);
        loop {
            tracing::warn!(
                attempt,
                retry_in_ms = retry_in.as_millis() as u64,
                "recovering managed server connection"
            );
            if events
                .send(ManagedEvent::Recovering(RecoveryStatus {
                    attempt,
                    retry_in,
                }))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(retry_in).await;

            // Every attempt holds to the stop this client set out from, never
            // to one recorded since: a Manual stop recorded while it was away
            // is as final for it as for the clients that saw it happen.
            if let Some(stopped) = baseline.stopped_since(&config) {
                leave_on_manual_stop(&events, stopped).await;
                return;
            }
            let deadline = tokio::time::Instant::now() + config.startup_timeout;
            let recovery = if configured_build_can_restore {
                establish_connection(&config, &http, deadline, baseline).await
            } else {
                match wait_for_protocol_compatible_connection(
                    &config, &http, None, deadline, baseline,
                )
                .await
                {
                    ConnectionWait::Ready(replacement) => Ok(*replacement),
                    ConnectionWait::Stopped(stopped) => {
                        leave_on_manual_stop(&events, stopped).await;
                        return;
                    }
                    ConnectionWait::Incompatible(protocol_version) => {
                        let message = format!(
                            "recovery server protocol version {protocol_version} is incompatible with client protocol version {PROTOCOL_VERSION}"
                        );
                        tracing::error!("{message}");
                        let _ = events.send(ManagedEvent::Fatal(message)).await;
                        return;
                    }
                    ConnectionWait::TimedOut => Err(anyhow!("compatible Suru server is not ready")),
                }
            };
            match recovery {
                Ok(recovered) => {
                    connection = recovered;
                    break;
                }
                // A Manual stop recorded during the attempt — ending the
                // server it launched, or the one it found ready before its
                // streams opened — means the user stopped the Channel while
                // this client was restoring it. That stop is as final here as
                // for the clients attached to the server it stopped, so this
                // client leaves as they do rather than launching again.
                Err(error) => match error.downcast::<launcher::StoppedDuringLaunch>() {
                    Ok(stopped) => {
                        leave_on_manual_stop(&events, stopped.instance_id).await;
                        return;
                    }
                    Err(_) => {
                        attempt = attempt.saturating_add(1);
                        retry_in = backoff.next();
                    }
                },
            }
        }
    }
}

/// Tells the client that the server `instance_id` was stopped manually,
/// as that server's final event tells the clients attached to it, so it
/// leaves the same way.
async fn leave_on_manual_stop(events: &mpsc::Sender<ManagedEvent>, instance_id: uuid::Uuid) {
    tracing::info!(%instance_id, "the channel was stopped while this client recovered its server");
    let _ = events
        .send(ManagedEvent::ServerShutdown(ServerShutdown {
            instance_id,
            reason: ShutdownReason::Manual,
        }))
        .await;
}

enum ConnectionProbe {
    Pending,
    Ready(Box<ActiveConnection>),
    Incompatible(u32),
    /// A Manual stop of this instance was recorded since the client's
    /// baseline.
    Stopped(uuid::Uuid),
}

enum ActiveStreamResult {
    Lifecycle(Result<StreamOutcome>),
    Catalog(Result<session_catalog_stream::StreamOutcome>),
}

enum ConnectionWait {
    Ready(Box<ActiveConnection>),
    Incompatible(u32),
    TimedOut,
    /// A Manual stop of this instance was recorded since the client's
    /// baseline, which ends the wait as it ends the client.
    Stopped(uuid::Uuid),
}

/// Waits for a server other than `excluded_instance_id` — the successor a
/// replacement promised, or whatever another client restored — launching
/// none itself, and holding to `baseline` throughout: a Manual stop recorded
/// since ends the wait, whether it stopped the successor while this client
/// was opening its streams or came before a server its user started again.
async fn wait_for_protocol_compatible_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    excluded_instance_id: Option<uuid::Uuid>,
    deadline: tokio::time::Instant,
    baseline: launcher::StopBaseline,
) -> ConnectionWait {
    loop {
        match probe_protocol_compatible_connection(
            config,
            http,
            excluded_instance_id,
            deadline,
            baseline,
        )
        .await
        {
            ConnectionProbe::Ready(connection) => return ConnectionWait::Ready(connection),
            ConnectionProbe::Incompatible(protocol_version) => {
                return ConnectionWait::Incompatible(protocol_version);
            }
            ConnectionProbe::Stopped(stopped) => return ConnectionWait::Stopped(stopped),
            ConnectionProbe::Pending => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return ConnectionWait::TimedOut;
        }
        tokio::time::sleep_until((now + Duration::from_millis(50)).min(deadline)).await;
    }
}

async fn probe_protocol_compatible_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    excluded_instance_id: Option<uuid::Uuid>,
    deadline: tokio::time::Instant,
    baseline: launcher::StopBaseline,
) -> ConnectionProbe {
    let probed = tokio::time::timeout_at(deadline, lifecycle::probe(config)).await;
    // Decided before whatever the probe found is attached to.
    if let Some(stopped) = baseline.stopped_since(config) {
        return ConnectionProbe::Stopped(stopped);
    }
    let Ok(Ok(Registration { descriptor, health })) = probed else {
        return ConnectionProbe::Pending;
    };
    if excluded_instance_id == Some(descriptor.instance_id) {
        return ConnectionProbe::Pending;
    }
    if excluded_instance_id == Some(health.instance_id) || health.lifecycle != LifecycleState::Ready
    {
        return ConnectionProbe::Pending;
    }
    if health.protocol_version != PROTOCOL_VERSION {
        return ConnectionProbe::Incompatible(health.protocol_version);
    }
    let Ok(streams) =
        open_managed_streams(http, &descriptor, deadline, config.startup_timeout).await
    else {
        // The server found ready may have been stopped manually while its
        // streams opened, and then that stop is why they did not.
        return baseline
            .stopped_since(config)
            .map_or(ConnectionProbe::Pending, ConnectionProbe::Stopped);
    };
    ConnectionProbe::Ready(Box::new(ActiveConnection {
        descriptor,
        health,
        lifecycle_response: streams.lifecycle,
        catalog_response: streams.catalog,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_client::{INITIAL_RECOVERY_BACKOFF, MAX_RECOVERY_BACKOFF};

    #[test]
    fn default_recovery_backoff_doubles_from_fifty_millis_and_caps_at_five_seconds() {
        let mut waits = Vec::new();
        let mut backoff = RecoveryBackoff::new(INITIAL_RECOVERY_BACKOFF, MAX_RECOVERY_BACKOFF);
        waits.extend((0..10).map(|_| backoff.next()));
        assert_eq!(
            waits,
            [50, 100, 200, 400, 800, 1600, 3200, 5000, 5000, 5000]
                .map(Duration::from_millis)
                .to_vec()
        );
        assert_eq!(INITIAL_RECOVERY_BACKOFF, Duration::from_millis(50));
        assert_eq!(MAX_RECOVERY_BACKOFF, Duration::from_secs(5));
    }
}
