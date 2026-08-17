use chidori::{
    managed_client::{ManagedEvent, RecoveryStatus},
    protocol::{CounterSnapshot, Health, LifecycleState},
    tui::{TuiState, render},
};
use ratatui::{Terminal, backend::TestBackend};
use std::time::Duration;
use uuid::Uuid;

fn rendered_rows(state: &TuiState) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(80, 15)).expect("create test terminal");
    terminal
        .draw(|frame| render(frame, state))
        .expect("render TUI state");
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn connecting_view_exposes_connection_state_before_a_snapshot_arrives() {
    let screen = rendered_rows(&TuiState::default()).join("\n");

    assert!(screen.contains("--"));
    assert!(screen.contains("Connecting to Chidori server..."));
}

#[test]
fn connected_view_centers_the_counter_and_shows_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(Health {
        instance_id,
        pid: 42_424,
        lifecycle: LifecycleState::Ready,
        protocol_version: 1,
        build_identity: "chidori@test".to_owned(),
    }));
    state.apply(ManagedEvent::Snapshot(CounterSnapshot {
        instance_id,
        value: 17,
        revision: 17,
    }));

    let rendered = rendered_rows(&state);

    let counter_position = rendered
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.chars()
                .collect::<Vec<_>>()
                .windows(2)
                .position(|pair| pair == ['1', '7'])
                .map(|column| (row, column))
        })
        .expect("render counter");
    assert_eq!(counter_position.0, 6);
    assert!((39..=40).contains(&counter_position.1));

    let screen = rendered.join("\n");
    assert!(screen.contains("Connected"));
    assert!(screen.contains("pid 42424"));
    assert!(screen.contains("c2f03bd2"));
}

#[test]
fn recovering_view_retains_the_last_known_counter_and_server_identity() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(Health {
        instance_id,
        pid: 42_424,
        lifecycle: LifecycleState::Ready,
        protocol_version: 1,
        build_identity: "chidori@test".to_owned(),
    }));
    state.apply(ManagedEvent::Snapshot(CounterSnapshot {
        instance_id,
        value: 17,
        revision: 17,
    }));

    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 2,
        retry_in: Duration::from_millis(500),
    }));

    let screen = rendered_rows(&state).join("\n");

    assert!(screen.contains("17"));
    assert!(screen.contains("Recovering"));
    assert!(screen.contains("pid 42424"));
}

#[test]
fn recovered_view_switches_identity_and_counter_together_on_the_fresh_snapshot() {
    let previous_instance_id = Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002")
        .expect("parse previous instance ID");
    let recovered_instance_id = Uuid::parse_str("a4cc72ad-5507-4d4f-89f4-a3f7f1119d41")
        .expect("parse recovered instance ID");
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(Health {
        instance_id: previous_instance_id,
        pid: 42_424,
        lifecycle: LifecycleState::Ready,
        protocol_version: 1,
        build_identity: "chidori@test".to_owned(),
    }));
    state.apply(ManagedEvent::Snapshot(CounterSnapshot {
        instance_id: previous_instance_id,
        value: 17,
        revision: 17,
    }));
    state.apply(ManagedEvent::Recovering(RecoveryStatus {
        attempt: 1,
        retry_in: Duration::ZERO,
    }));
    state.apply(ManagedEvent::Connected(Health {
        instance_id: recovered_instance_id,
        pid: 84_848,
        lifecycle: LifecycleState::Ready,
        protocol_version: 1,
        build_identity: "chidori@test".to_owned(),
    }));

    let awaiting_snapshot = rendered_rows(&state).join("\n");
    assert!(awaiting_snapshot.contains("Recovering"));
    assert!(awaiting_snapshot.contains("17"));
    assert!(awaiting_snapshot.contains("pid 42424"));
    assert!(!awaiting_snapshot.contains("84848"));

    state.apply(ManagedEvent::Snapshot(CounterSnapshot {
        instance_id: recovered_instance_id,
        value: 1,
        revision: 1,
    }));
    let recovered = rendered_rows(&state).join("\n");
    assert!(recovered.contains("Connected"));
    assert!(recovered.contains("pid 84848"));
    assert!(recovered.contains("a4cc72ad"));
    assert!(!recovered.contains("17"));
}

#[test]
fn fatal_protocol_error_is_rendered_visibly_with_the_last_known_state() {
    let instance_id =
        Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID");
    let mut state = TuiState::default();
    state.apply(ManagedEvent::Connected(Health {
        instance_id,
        pid: 42_424,
        lifecycle: LifecycleState::Ready,
        protocol_version: 1,
        build_identity: "chidori@test".to_owned(),
    }));
    state.apply(ManagedEvent::Snapshot(CounterSnapshot {
        instance_id,
        value: 17,
        revision: 17,
    }));

    state.apply(ManagedEvent::Fatal(
        "server sent unknown event type 'future_event'".to_owned(),
    ));

    let screen = rendered_rows(&state).join("\n");
    assert!(screen.contains("17"));
    assert!(screen.contains("Connection failed"));
    assert!(screen.contains("unknown event type 'future_event'"));
}
