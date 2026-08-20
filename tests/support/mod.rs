use suru::{
    managed_client::{ManagedClient, ManagedEvent},
    protocol::{Health, RuntimeDescriptor, ServerShutdown, ShutdownReason},
};
use tokio::time::{Duration, timeout};

pub fn write_runtime_descriptor(path: impl AsRef<std::path::Path>, descriptor: &RuntimeDescriptor) {
    let path = path.as_ref();
    serde_json::to_writer(
        std::fs::File::create(path)
            .unwrap_or_else(|error| panic!("create runtime descriptor {path:?}: {error}")),
        descriptor,
    )
    .unwrap_or_else(|error| panic!("write runtime descriptor {path:?}: {error}"));
}

pub fn read_runtime_descriptor(path: impl AsRef<std::path::Path>) -> RuntimeDescriptor {
    let path = path.as_ref();
    serde_json::from_reader(
        std::fs::File::open(path)
            .unwrap_or_else(|error| panic!("open runtime descriptor {path:?}: {error}")),
    )
    .unwrap_or_else(|error| panic!("decode runtime descriptor {path:?}: {error}"))
}

pub async fn receive_initial_state(client: &mut ManagedClient) -> Health {
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next())
            .await
            .expect("connecting event arrives"),
        Some(ManagedEvent::Connecting)
    ));
    let connected = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("connected event arrives")
        .expect("managed client remains open");
    let ManagedEvent::Connected(identity) = connected else {
        panic!("expected connected event, got {connected:?}");
    };
    identity
}

pub async fn request_server_shutdown(
    descriptor: &RuntimeDescriptor,
    reason: ShutdownReason,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: descriptor.instance_id,
            reason,
        })
        .send()
        .await
        .expect("request authenticated server shutdown")
}
