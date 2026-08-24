//! Fixtures shared by more than one area of the session protocol tests.

use suru::{
    managed_client::{ManagedClient, SessionEvent},
    protocol::{
        AgentSelection, CreateSessionRequest, ModelAvailability, ModelCatalog, ModelDescriptor,
        ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue,
        ProviderId, RuntimeDescriptor, SessionId, SessionRevision, SessionSnapshot, SessionUpdate,
    },
};
use tokio::time::{Duration, timeout};

/// One Provider's default Model, named after the Provider that serves it, which
/// is all any multi-Provider test needs of a catalog.
pub fn hosted_model(provider: &str, model: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(model),
        display_name: model.to_owned(),
        description: String::new(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: Vec::new(),
    }
}

pub fn hosted_selection(provider: &str, model: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
        options: Vec::new(),
    }
}

pub async fn create_session(
    descriptor: &RuntimeDescriptor,
    request: &CreateSessionRequest,
) -> SessionSnapshot {
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session")
}

pub async fn read_session(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> SessionSnapshot {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session")
        .error_for_status()
        .expect("Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session")
}

pub async fn list_catalog(descriptor: &RuntimeDescriptor) -> ModelCatalog {
    reqwest::Client::new()
        .get(format!("{}/v1/models", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Models")
        .error_for_status()
        .expect("Model listing succeeds")
        .json::<ModelCatalog>()
        .await
        .expect("decode Model catalog")
}

pub async fn refresh_catalog(descriptor: &RuntimeDescriptor) -> ModelCatalog {
    reqwest::Client::new()
        .post(format!("{}/v1/models/refresh", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("refresh Model catalog")
        .error_for_status()
        .expect("Model refresh succeeds")
        .json::<ModelCatalog>()
        .await
        .expect("decode refreshed Model catalog")
}

pub fn controlled_selection(model: &str, effort: &str, speed: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new(model),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(speed),
                },
            },
        ],
    }
}

pub async fn next_session_update(
    subscription: &mut suru::managed_client::SessionSubscription,
) -> SessionUpdate {
    let SessionEvent::Updated(update) = timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("Session update is valid")
    else {
        panic!("expected a Session update");
    };
    update
}

pub async fn read_session_at_least_revision(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    revision: SessionRevision,
) -> SessionSnapshot {
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = client
                .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("read Session while awaiting revision")
                .error_for_status()
                .expect("Session remains readable")
                .json::<SessionSnapshot>()
                .await
                .expect("decode Session while awaiting revision");
            if snapshot.revision >= revision {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Session reaches expected revision")
}

pub async fn receive_managed_client_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::Connecting)
    ));
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::Connected(_))
    ));
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::SettingsSnapshot(_))
    ));
}
