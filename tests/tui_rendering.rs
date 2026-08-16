use chidori::{
    managed_client::ManagedEvent,
    protocol::{CounterSnapshot, Health, LifecycleState},
    tui::{TuiState, render},
};
use ratatui::{Terminal, backend::TestBackend};
use uuid::Uuid;

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

    let mut terminal = Terminal::new(TestBackend::new(80, 15)).expect("create test terminal");
    terminal
        .draw(|frame| render(frame, &state))
        .expect("render connected view");

    let buffer = terminal.backend().buffer();
    let rendered = buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();

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
