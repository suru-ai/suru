//! Managed connection establishment, replacement, and crash recovery orchestration.

use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Health, LifecycleState, PROTOCOL_VERSION, RuntimeDescriptor};

use super::{
    ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, STARTUP_TIMEOUT,
    event_stream::{self, StreamOutcome},
    launcher,
    lifecycle::{self, Registration},
};

const INITIAL_RECOVERY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);

struct ActiveConnection {
    descriptor: RuntimeDescriptor,
    health: Health,
    response: reqwest::Response,
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
    let response =
        match tokio::time::timeout_at(deadline, event_stream::open(http, &descriptor)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) if error.status().is_some() => {
                return Err(launcher::startup_error(
                    config,
                    &format!("server rejected the initial event stream: {error}"),
                ));
            }
            Ok(Err(error)) => {
                return Err(launcher::startup_error(
                    config,
                    &format!("could not open the initial event stream: {error}"),
                ));
            }
            Err(_) => {
                return Err(launcher::startup_error(
                    config,
                    "initial event stream did not open within 15s",
                ));
            }
        };
    Ok(ActiveConnection {
        descriptor,
        health,
        response,
    })
}

async fn run_managed_client(
    config: ManagedClientConfig,
    http: reqwest::Client,
    mut connection: ActiveConnection,
    events: mpsc::Sender<ManagedEvent>,
    descriptor: watch::Sender<RuntimeDescriptor>,
) {
    if events.send(ManagedEvent::Connecting).await.is_err() {
        return;
    }
    loop {
        descriptor.send_replace(connection.descriptor.clone());
        let active_instance_id = connection.descriptor.instance_id;
        let active_build_identity = connection.health.build_identity.clone();
        if events
            .send(ManagedEvent::Connected(connection.health))
            .await
            .is_err()
        {
            return;
        }
        let replaced_instance_id =
            match event_stream::consume(connection.response, &events, active_instance_id).await {
                Ok(StreamOutcome::Disconnected) => None,
                Ok(StreamOutcome::ManualShutdown) => return,
                Ok(StreamOutcome::Replacement { instance_id }) => Some(instance_id),
                Ok(StreamOutcome::ReceiverClosed) => return,
                Err(error) => {
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
    let Ok(Ok(response)) =
        tokio::time::timeout_at(deadline, event_stream::open(http, &descriptor)).await
    else {
        return ConnectionProbe::Pending;
    };
    ConnectionProbe::Ready(Box::new(ActiveConnection {
        descriptor,
        health,
        response,
    }))
}

fn next_recovery_backoff(previous: Duration) -> Duration {
    if previous.is_zero() {
        INITIAL_RECOVERY_BACKOFF
    } else {
        previous.saturating_mul(2).min(MAX_RECOVERY_BACKOFF)
    }
}
