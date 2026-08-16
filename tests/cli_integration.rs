use std::process::Command;

use chidori::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::RuntimeDescriptor,
};
use sysinfo::{Pid, System};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn server_start_returns_after_a_detached_server_is_ready() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "detached-start-test";
    let output = Command::new(env!("CARGO_BIN_EXE_chidori"))
        .args(["server", "start"])
        .env("CHIDORI_STATE_DIR", state_dir.path())
        .env("CHIDORI_CHANNEL", channel)
        .output()
        .expect("run server start command");

    assert!(
        output.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut client = ManagedClient::connect(ManagedClientConfig::new(state_dir.path(), channel))
        .await
        .expect("connect after start command has exited");
    let event = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("connected event arrives")
        .expect("managed client remains open");
    let ManagedEvent::Connected(identity) = event else {
        panic!("expected connected event, got {event:?}");
    };
    assert_ne!(identity.pid, std::process::id());

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

fn stop_test_server(state_dir: &std::path::Path, channel: &str) {
    let descriptor_path = state_dir.join(channel).join("runtime.json");
    let descriptor: RuntimeDescriptor = serde_json::from_reader(
        std::fs::File::open(descriptor_path).expect("open test server descriptor"),
    )
    .expect("decode test server descriptor");
    let mut system = System::new_all();
    system.refresh_all();
    let process = system
        .process(Pid::from_u32(descriptor.pid))
        .expect("find detached test server");
    assert!(process.kill(), "stop detached test server");
}
