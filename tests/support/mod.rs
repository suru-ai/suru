use chidori::{
    managed_client::{ManagedClient, ManagedEvent},
    protocol::{CounterSnapshot, Health},
};
use tokio::time::{Duration, timeout};

pub async fn receive_initial_state(client: &mut ManagedClient) -> (Health, CounterSnapshot) {
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
    let snapshot = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("snapshot arrives")
        .expect("managed client remains open");
    let ManagedEvent::Snapshot(snapshot) = snapshot else {
        panic!("expected snapshot event, got {snapshot:?}");
    };
    (identity, snapshot)
}
