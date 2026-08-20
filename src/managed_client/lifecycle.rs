//! Runtime registration inspection and instance-specific lifecycle control.

use std::{
    fs::{File, OpenOptions},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;

use crate::protocol::{Health, RuntimeDescriptor, ServerShutdown, ShutdownReason};

use super::ManagedClientConfig;

const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Registration {
    pub(super) descriptor: RuntimeDescriptor,
    pub(super) health: Health,
}

pub(super) enum RegistrationInspection {
    Missing,
    Live(Registration),
    Stale(String),
    Unreachable(String),
}

pub(super) async fn inspect_registration(
    config: &ManagedClientConfig,
) -> Result<RegistrationInspection> {
    let descriptor_path = config.descriptor_path();
    if !descriptor_path
        .try_exists()
        .context("inspect runtime descriptor")?
    {
        return Ok(RegistrationInspection::Missing);
    }
    let descriptor = match read_descriptor(&descriptor_path) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            if !descriptor_path
                .try_exists()
                .context("reinspect runtime descriptor")?
            {
                return Ok(RegistrationInspection::Missing);
            }
            return Ok(RegistrationInspection::Stale(format!("{error:#}")));
        }
    };
    if let Err(error) = validate_loopback_url(&descriptor.base_url) {
        return Ok(RegistrationInspection::Stale(format!("{error:#}")));
    }
    match tokio::time::timeout(STATUS_TIMEOUT, inspect_health(&descriptor)).await {
        Ok(Ok(health)) => Ok(RegistrationInspection::Live(Registration {
            descriptor,
            health,
        })),
        Ok(Err(HealthInspectionError::Stale(reason))) => Ok(RegistrationInspection::Stale(reason)),
        Ok(Err(HealthInspectionError::Unreachable(reason))) => {
            Ok(RegistrationInspection::Unreachable(reason))
        }
        Err(_) => Ok(RegistrationInspection::Unreachable(
            "authenticated health check timed out".to_owned(),
        )),
    }
}

pub(super) async fn probe(config: &ManagedClientConfig) -> Result<Registration> {
    let descriptor = read_descriptor(&config.descriptor_path())?;
    validate_loopback_url(&descriptor.base_url)?;
    let health = inspect_descriptor_health(&descriptor).await?;
    Ok(Registration { descriptor, health })
}

pub(super) async fn inspect_descriptor_health(descriptor: &RuntimeDescriptor) -> Result<Health> {
    inspect_health(descriptor)
        .await
        .map_err(|error| anyhow!(error.reason()))
}

pub(super) async fn shutdown_registered_instance(
    config: &ManagedClientConfig,
    registration: &Registration,
    reason: ShutdownReason,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let policy = shutdown_policy(reason, deadline);
    let instance_id = registration.health.instance_id;
    let request = ServerShutdown {
        instance_id,
        reason,
    };
    let response = tokio::time::timeout_at(
        policy.request_deadline,
        reqwest::Client::new()
            .post(format!(
                "{}/v1/server/stop",
                registration.descriptor.base_url
            ))
            .bearer_auth(&registration.descriptor.token)
            .json(&request)
            .send(),
    )
    .await
    .with_context(|| format!("{} request timed out", policy.action));
    match response {
        Ok(Ok(response)) if response.status() == reqwest::StatusCode::CONFLICT => {
            if !policy.transition_races_are_expected {
                bail!("registered Chidori server changed before it could be stopped");
            }
        }
        Ok(Ok(response)) => {
            response
                .error_for_status()
                .with_context(|| format!("server rejected {} request", policy.action))?;
        }
        Ok(Err(_)) if policy.transition_races_are_expected => {}
        Ok(Err(error)) => {
            return Err(error).with_context(|| format!("send {} request", policy.action));
        }
        Err(error) => return Err(error),
    }

    let mut target_stopped = false;
    loop {
        if !target_stopped {
            target_stopped = matches!(
                tokio::time::timeout_at(deadline, inspect_health(&registration.descriptor)).await,
                Ok(Err(HealthInspectionError::Unreachable(_)))
            );
        }
        let registration_released = match read_descriptor(&config.descriptor_path()) {
            Ok(current) => current.instance_id != instance_id,
            Err(_) => !config.descriptor_path().exists(),
        };
        let channel_released = !policy.wait_for_channel_release || !channel_is_owned(config)?;
        if target_stopped && registration_released && channel_released {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(policy.timeout_message)
        }
        tokio::time::sleep_until((now + Duration::from_millis(25)).min(deadline)).await;
    }
}

pub(super) fn channel_is_owned(config: &ManagedClientConfig) -> Result<bool> {
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.lock_path())
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect server election lock"),
    };
    Ok(lock.try_lock_exclusive().is_err())
}

struct ShutdownPolicy {
    request_deadline: tokio::time::Instant,
    transition_races_are_expected: bool,
    wait_for_channel_release: bool,
    action: &'static str,
    timeout_message: &'static str,
}

fn shutdown_policy(reason: ShutdownReason, deadline: tokio::time::Instant) -> ShutdownPolicy {
    match reason {
        ShutdownReason::Manual => ShutdownPolicy {
            request_deadline: (tokio::time::Instant::now() + STATUS_TIMEOUT).min(deadline),
            transition_races_are_expected: false,
            wait_for_channel_release: false,
            action: "manual stop",
            timeout_message: "Chidori server did not stop within 5s",
        },
        ShutdownReason::Replacement => ShutdownPolicy {
            request_deadline: deadline,
            transition_races_are_expected: true,
            wait_for_channel_release: true,
            action: "replacement stop",
            timeout_message: "mismatched Chidori server did not release the channel before startup timed out",
        },
    }
}

enum HealthInspectionError {
    Stale(String),
    Unreachable(String),
}

impl HealthInspectionError {
    fn reason(self) -> String {
        match self {
            Self::Stale(reason) | Self::Unreachable(reason) => reason,
        }
    }
}

async fn inspect_health(
    descriptor: &RuntimeDescriptor,
) -> std::result::Result<Health, HealthInspectionError> {
    let response = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .map_err(|error| HealthInspectionError::Unreachable(error.to_string()))?;
    if !response.status().is_success() {
        return Err(HealthInspectionError::Stale(format!(
            "authenticated health request returned {}",
            response.status()
        )));
    }
    let health = response.json::<Health>().await.map_err(|error| {
        HealthInspectionError::Stale(format!("invalid health response: {error}"))
    })?;
    if health.identity != descriptor.identity {
        return Err(HealthInspectionError::Stale(
            "registered server identity does not match its runtime descriptor".to_owned(),
        ));
    }
    Ok(health)
}

fn read_descriptor(path: &Path) -> Result<RuntimeDescriptor> {
    let file = File::open(path)
        .with_context(|| format!("no running Chidori server was found at {path:?}"))?;
    serde_json::from_reader(file).context("decode runtime descriptor")
}

fn validate_loopback_url(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url).context("runtime descriptor has an invalid URL")?;
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") || url.port().is_none() {
        bail!("runtime descriptor URL is not an HTTP IPv4 loopback address");
    }
    Ok(())
}
