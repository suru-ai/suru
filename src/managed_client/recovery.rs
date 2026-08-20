//! Managed connection establishment, replacement, and crash recovery orchestration.

use std::{collections::HashSet, time::Duration};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Health, LifecycleState, PROTOCOL_VERSION, RuntimeDescriptor};

use super::{
    ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, STARTUP_TIMEOUT,
    event_stream::{self, StreamOutcome},
    launcher,
    lifecycle::{self, Registration},
    session_catalog_stream,
};

const INITIAL_RECOVERY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);

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
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let http = reqwest::Client::new();
    let connection = establish_connection(&config, &http, deadline).await?;
    let (events_tx, events_rx) = mpsc::channel(32);
    let (descriptor_tx, descriptor_rx) = watch::channel(connection.descriptor.clone());
    let managed_http = http.clone();
    let task = tokio::spawn(run_managed_client(
        config,
        managed_http,
        connection,
        events_tx,
        descriptor_tx,
    ));
    Ok(ManagedClient {
        events: events_rx,
        http,
        descriptor: descriptor_rx,
        task,
    })
}

async fn establish_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    deadline: tokio::time::Instant,
) -> Result<ActiveConnection> {
    let Registration { descriptor, health } = launcher::ensure_server(config, deadline).await?;
    let streams = open_managed_streams(http, &descriptor, deadline)
        .await
        .map_err(|error| launcher::startup_error(config, &error.to_string()))?;
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
) -> Result<ManagedStreamResponses> {
    let lifecycle = open_required_stream(
        deadline,
        "event stream",
        event_stream::open(http, descriptor),
    )
    .await?;
    let catalog = open_required_stream(
        deadline,
        "Session catalog stream",
        session_catalog_stream::open(http, descriptor),
    )
    .await?;
    Ok(ManagedStreamResponses { lifecycle, catalog })
}

async fn open_required_stream(
    deadline: tokio::time::Instant,
    name: &str,
    opening: impl std::future::Future<Output = reqwest::Result<reqwest::Response>>,
) -> Result<reqwest::Response> {
    match tokio::time::timeout_at(deadline, opening).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) if error.status().is_some() => {
            Err(anyhow!("server rejected the initial {name}: {error}"))
        }
        Ok(Err(error)) => Err(anyhow!("could not open the initial {name}: {error}")),
        Err(_) => Err(anyhow!("initial {name} did not open within 15s")),
    }
}

async fn run_managed_client(
    config: ManagedClientConfig,
    http: reqwest::Client,
    mut connection: ActiveConnection,
    events: mpsc::Sender<ManagedEvent>,
    descriptor: watch::Sender<RuntimeDescriptor>,
) {
    let mut known_session_ids: Option<HashSet<_>> = None;
    if events.send(ManagedEvent::Connecting).await.is_err() {
        return;
    }
    loop {
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
        );
        tokio::pin!(catalog);
        let stream_result = tokio::select! {
            biased;
            outcome = &mut catalog => Some(ActiveStreamResult::Catalog(outcome)),
            hydrated = catalog_hydration => {
                if hydrated.is_err() {
                    return;
                }
                None
            }
        };
        let stream_result = match stream_result {
            Some(stream_result) => stream_result,
            None => {
                if events
                    .send(ManagedEvent::Connected(connection.health))
                    .await
                    .is_err()
                {
                    return;
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
                let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                return;
            }
        };

        if let Some(replaced_instance_id) = replaced_instance_id {
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
            let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
            match wait_for_protocol_compatible_connection(
                &config,
                &http,
                Some(replaced_instance_id),
                deadline,
            )
            .await
            {
                ConnectionWait::Ready(replacement) => {
                    connection = *replacement;
                    continue;
                }
                ConnectionWait::Incompatible(protocol_version) => {
                    let _ = events
                        .send(ManagedEvent::Fatal(format!(
                            "replacement server protocol version {protocol_version} is incompatible with client protocol version {PROTOCOL_VERSION}"
                        )))
                        .await;
                    return;
                }
                ConnectionWait::TimedOut => {
                    let _ = events
                        .send(ManagedEvent::Fatal(
                            "replacement Chidori server did not become ready within 15s".to_owned(),
                        ))
                        .await;
                    return;
                }
            }
        }

        let configured_build_can_restore =
            crate::build_identity::for_executable(&config.server_executable)
                .is_ok_and(|identity| identity == active_build_identity);
        let mut attempt = 1;
        let mut retry_in = Duration::ZERO;
        loop {
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

            let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
            let recovery = if configured_build_can_restore {
                establish_connection(&config, &http, deadline).await
            } else {
                match wait_for_protocol_compatible_connection(&config, &http, None, deadline).await
                {
                    ConnectionWait::Ready(replacement) => Ok(*replacement),
                    ConnectionWait::Incompatible(protocol_version) => {
                        let _ = events
                            .send(ManagedEvent::Fatal(format!(
                                "recovery server protocol version {protocol_version} is incompatible with client protocol version {PROTOCOL_VERSION}"
                            )))
                            .await;
                        return;
                    }
                    ConnectionWait::TimedOut => {
                        Err(anyhow!("compatible Chidori server is not ready"))
                    }
                }
            };
            match recovery {
                Ok(recovered) => {
                    connection = recovered;
                    break;
                }
                Err(_) => {
                    attempt = attempt.saturating_add(1);
                    retry_in = next_recovery_backoff(retry_in);
                }
            }
        }
    }
}

enum ConnectionProbe {
    Pending,
    Ready(Box<ActiveConnection>),
    Incompatible(u32),
}

enum ActiveStreamResult {
    Lifecycle(Result<StreamOutcome>),
    Catalog(Result<session_catalog_stream::StreamOutcome>),
}

enum ConnectionWait {
    Ready(Box<ActiveConnection>),
    Incompatible(u32),
    TimedOut,
}

async fn wait_for_protocol_compatible_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    excluded_instance_id: Option<uuid::Uuid>,
    deadline: tokio::time::Instant,
) -> ConnectionWait {
    loop {
        match probe_protocol_compatible_connection(config, http, excluded_instance_id, deadline)
            .await
        {
            ConnectionProbe::Ready(connection) => return ConnectionWait::Ready(connection),
            ConnectionProbe::Incompatible(protocol_version) => {
                return ConnectionWait::Incompatible(protocol_version);
            }
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
) -> ConnectionProbe {
    let Ok(Ok(Registration { descriptor, health })) =
        tokio::time::timeout_at(deadline, lifecycle::probe(config)).await
    else {
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
    let Ok(streams) = open_managed_streams(http, &descriptor, deadline).await else {
        return ConnectionProbe::Pending;
    };
    ConnectionProbe::Ready(Box::new(ActiveConnection {
        descriptor,
        health,
        lifecycle_response: streams.lifecycle,
        catalog_response: streams.catalog,
    }))
}

fn next_recovery_backoff(previous: Duration) -> Duration {
    if previous.is_zero() {
        INITIAL_RECOVERY_BACKOFF
    } else {
        previous.saturating_mul(2).min(MAX_RECOVERY_BACKOFF)
    }
}
