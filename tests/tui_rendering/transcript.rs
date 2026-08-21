//! Transcript projection, styling, and scroll navigation.

use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{
        buffer_rows, connected_application, failed_session_snapshot, navigable_session_snapshot,
        rendered_application_buffer, rendered_application_rows_at, rendered_row, text_position,
    },
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::{Buffer, Cell},
    style::{Color, Modifier},
};
use std::time::Duration;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent, SessionSubscription},
    protocol::{
        Activity, ActivityId, ActivityStatus, CreateSessionRequest, FileChange, InitialPrompt,
        Message, MessageId, MessageRole, MessageStatus, Prompt, PromptDelivery, PromptId,
        PromptOrder, PromptStatus, SessionChange, SessionId, SessionRevision, SessionStatus,
        SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    server::{AgentOutput, ServerConfig},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId},
};

#[track_caller]
fn assert_no_control_cells(buffer: &Buffer) {
    assert!(
        buffer.content().iter().all(|cell| {
            cell.symbol()
                .chars()
                .all(|character| !character.is_control() && character != '\u{7f}')
        }),
        "terminal control sequences leaked into rendered cells"
    );
}

fn text_cell<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
    buffer
        .cell(text_position(buffer, needle))
        .expect("rendered text position is inside the buffer")
}

async fn apply_next_session_event(
    application: &mut Application,
    subscription: &mut SessionSubscription,
) -> SessionEvent {
    let event = tokio::time::timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("Session event arrives")
        .expect("Session stream remains open")
        .expect("Session event is valid");
    application
        .handle_event(ApplicationEvent::Session(event.clone()))
        .expect("apply Session event to headless application");
    event
}

#[test]
fn terminal_input_capabilities_map_mouse_wheel_to_transcript_navigation() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 8),
        ))
        .expect("attach a long Session");
    let latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(latest.contains("Agent section 8"));

    let mouse_event = |kind| {
        InputEvent::Mouse(MouseEvent {
            kind,
            column: 12,
            row: 6,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert_eq!(
        application
            .handle_terminal_event(mouse_event(MouseEventKind::ScrollUp))
            .expect("scroll up through transcript content"),
        ApplicationTransition::Continue
    );
    let reading_history = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(reading_history.contains("Latest"));
    assert!(!reading_history.contains("Agent section 8"));

    assert_eq!(
        application
            .handle_terminal_event(mouse_event(MouseEventKind::ScrollDown))
            .expect("scroll down through transcript content"),
        ApplicationTransition::Continue
    );
}

#[test]
fn transcript_content_with_terminal_escapes_renders_sanitized_cells() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Agent)
        .expect("fixture contains an Agent message")
        .content = "Build finished: \x1b[32mok\x1b(B\x1b[m today".to_owned();
    snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::User)
        .expect("fixture contains a user Message")
        .content = "\x1b[31mPrompt section 1\x1b[0m".to_owned();
    let activity_id = ActivityId::new();
    snapshot.activities.push(Activity::Command {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Completed,
        command: "cargo test --all".to_owned(),
        cwd: None,
        output: concat!(
            "\x1b[1;31mtest result\x1b[22;32m: green-ok\x1b[0m. 78 passed;\r\n",
            "\tnext\x07\x1b[2K\x1b]0;hidden title\x07",
            "\x1bPdevice payload\x1b\\\x1b_hidden app data\x1b\\\0\x7fline"
        )
        .to_owned(),
        output_truncated: false,
        exit_status: Some(0),
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with escape-laden content");

    let buffer = rendered_application_buffer(&application, 90, 20);
    let screen = buffer_rows(&buffer).join("\n");
    assert!(screen.contains("Build finished: ok today"));
    assert!(screen.contains("Prompt section 1"));
    assert!(screen.contains("test result: green-ok. 78 passed;"));
    assert!(screen.contains("    nextline"));
    for hidden in ["hidden title", "device payload", "hidden app data"] {
        assert!(!screen.contains(hidden));
    }
    assert_no_control_cells(&buffer);
    assert_ne!(text_cell(&buffer, "Build finished").fg, Color::Green);
    assert_ne!(text_cell(&buffer, "Prompt section 1").fg, Color::Red);
    assert_eq!(text_cell(&buffer, "test result").fg, Color::LightRed);
    assert!(
        text_cell(&buffer, "test result")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(text_cell(&buffer, "green-ok").fg, Color::Green);
    assert!(
        !text_cell(&buffer, "green-ok")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(text_cell(&buffer, ". 78 passed").fg, Color::DarkGray);
}

#[test]
fn all_base_ansi_foregrounds_and_backgrounds_render_through_the_theme_palette() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let activity_id = ActivityId::new();
    let mut output = String::new();
    for (prefix, first_code) in [("nfg", 30), ("bfg", 90)] {
        for offset in 0..8 {
            output.push_str(&format!("\x1b[{}m{prefix}{offset} ", first_code + offset));
        }
        output.push('\n');
    }
    for (prefix, first_code) in [("nbg", 40), ("bbg", 100)] {
        output.push_str("\x1b[0m");
        for offset in 0..8 {
            output.push_str(&format!("\x1b[{}m{prefix}{offset} ", first_code + offset));
        }
        output.push('\n');
    }
    snapshot.activities.push(Activity::Command {
        id: activity_id,
        turn_id: snapshot.turns[0].id,
        status: ActivityStatus::Completed,
        command: "show ANSI palette".to_owned(),
        cwd: None,
        output,
        output_truncated: false,
        exit_status: Some(0),
    });
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with every base ANSI color");

    let buffer = rendered_application_buffer(&application, 160, 30);
    let normal = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    let bright = [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    for (index, expected) in normal.into_iter().enumerate() {
        assert_eq!(text_cell(&buffer, &format!("nfg{index}")).fg, expected);
        assert_eq!(text_cell(&buffer, &format!("nbg{index}")).bg, expected);
    }
    for (index, expected) in bright.into_iter().enumerate() {
        assert_eq!(text_cell(&buffer, &format!("bfg{index}")).fg, expected);
        assert_eq!(text_cell(&buffer, &format!("bbg{index}")).bg, expected);
    }
}

#[test]
fn escape_laden_transcript_stays_clean_after_scroll_and_session_switch() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut escaped = navigable_session_snapshot(SessionId::new(), workspace.path(), 8);
    let turn_id = escaped.turns[7].id;
    let activity_id = ActivityId::new();
    escaped.activities.push(Activity::Command {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Completed,
        command: "artifact-repro".to_owned(),
        cwd: None,
        output: "artifact marker \x1b[31mred\x1b[0m\x1b[2K\x1b]0;title\x07".to_owned(),
        output_truncated: false,
        exit_status: Some(0),
    });
    escaped
        .transcript
        .push(TranscriptItem::Activity { activity_id });
    application
        .handle_event(ApplicationEvent::SessionAttached(escaped))
        .expect("attach escape-laden Session");

    let mut terminal = Terminal::new(TestBackend::new(72, 18)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render escape-laden Session");
    let escaped_buffer = terminal.backend().buffer();
    assert!(
        buffer_rows(escaped_buffer)
            .join("\n")
            .contains("artifact marker")
    );
    assert_eq!(text_cell(escaped_buffer, "red").fg, Color::Red);
    assert_no_control_cells(escaped_buffer);
    application
        .handle_event(ApplicationEvent::Command(CommandId::ScrollTranscriptPageUp))
        .expect("scroll escape-laden Session");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render scrolled Session");

    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 1),
        ))
        .expect("switch to clean Session");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render clean Session after switch");
    let buffer = terminal.backend().buffer();
    let screen = buffer_rows(buffer).join("\n");
    assert!(screen.contains("Agent section 1"));
    assert!(!screen.contains("artifact marker"));
    assert_no_control_cells(buffer);
}

#[test]
fn activity_sgr_styles_patch_over_each_activity_base_style() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    let activities = [
        Activity::Status {
            id: ActivityId::new(),
            turn_id,
            text: "status \x1b[93;104mbright pair\x1b[0m plain status".to_owned(),
        },
        Activity::Error {
            id: ActivityId::new(),
            turn_id,
            text: "failure \x1b[4;7munder reversed\x1b[0m plain error".to_owned(),
        },
        Activity::Command {
            id: ActivityId::new(),
            turn_id,
            status: ActivityStatus::Completed,
            command: "colored-output".to_owned(),
            cwd: None,
            output: concat!(
                "\x1b[38;5;201;48;5;22mindexed pair\x1b[0m ",
                "\x1b[38;2;1;2;3;48;2;4;5;6mtruecolor pair\x1b[0m ",
                "\x1b[38:2::7:8:9;48:5:42mcolon pair\x1b[m ",
                "\x1b[2mdim text\x1b[0m \x1b[3mitalic text\x1b[0m"
            )
            .to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        },
    ];
    for activity in activities {
        snapshot.transcript.push(TranscriptItem::Activity {
            activity_id: activity.id(),
        });
        snapshot.activities.push(activity);
    }
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with SGR-styled Activities");

    let buffer = rendered_application_buffer(&application, 120, 28);
    assert_eq!(text_cell(&buffer, "bright pair").fg, Color::LightYellow);
    assert_eq!(text_cell(&buffer, "bright pair").bg, Color::LightBlue);
    assert_eq!(text_cell(&buffer, "plain status").fg, Color::DarkGray);

    let decorated_error = text_cell(&buffer, "under reversed");
    assert_eq!(decorated_error.fg, Color::Red);
    assert!(decorated_error.modifier.contains(Modifier::UNDERLINED));
    assert!(decorated_error.modifier.contains(Modifier::REVERSED));
    let plain_error = text_cell(&buffer, "plain error");
    assert_eq!(plain_error.fg, Color::Red);
    assert!(!plain_error.modifier.contains(Modifier::UNDERLINED));
    assert!(!plain_error.modifier.contains(Modifier::REVERSED));

    let indexed = text_cell(&buffer, "indexed pair");
    assert_eq!(indexed.fg, Color::Indexed(201));
    assert_eq!(indexed.bg, Color::Indexed(22));
    let truecolor = text_cell(&buffer, "truecolor pair");
    assert_eq!(truecolor.fg, Color::Rgb(1, 2, 3));
    assert_eq!(truecolor.bg, Color::Rgb(4, 5, 6));
    let colon = text_cell(&buffer, "colon pair");
    assert_eq!(colon.fg, Color::Rgb(7, 8, 9));
    assert_eq!(colon.bg, Color::Indexed(42));
    assert!(
        text_cell(&buffer, "dim text")
            .modifier
            .contains(Modifier::DIM)
    );
    assert!(
        text_cell(&buffer, "italic text")
            .modifier
            .contains(Modifier::ITALIC)
    );
}

#[test]
fn command_output_osc_8_hyperlinks_render_with_link_style() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let activity = Activity::Command {
        id: ActivityId::new(),
        turn_id: snapshot.turns[0].id,
        status: ActivityStatus::Completed,
        command: "show links".to_owned(),
        cwd: None,
        output: concat!(
            "BEL: \x1b]8;id=bel;https://example.com/bel\x07bel link\x1b]8;;\x07 ",
            "ST: \x1b]8;;https://example.com/st\x1b\\st link\x1b]8;;\x1b\\"
        )
        .to_owned(),
        output_truncated: false,
        exit_status: Some(0),
    };
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: activity.id(),
    });
    snapshot.activities.push(activity);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with OSC 8-linked command output");

    let buffer = rendered_application_buffer(&application, 100, 24);
    for link_text in ["bel link", "st link"] {
        let cell = text_cell(&buffer, link_text);
        assert_eq!(cell.fg, Color::Blue);
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
    }
    assert_no_control_cells(&buffer);
}

#[tokio::test]
async fn streamed_agent_markdown_updates_one_unboxed_row_through_the_real_session_stream() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "headless-agent-stream-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "headless-agent-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let mut application = Application::new(workspace.path());
    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the stream\nwhile keeping this deliberately long user Message elevated across every wrapped continuation of the transcript block, including its semantic left accent"
                    .to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session");
    assert!(matches!(
        apply_next_session_event(&mut application, &mut subscription).await,
        SessionEvent::Snapshot(_)
    ));

    server
        .session_event_sink()
        .publish(
            session_id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Continue with an active Agent".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id,
                        agent: None,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Continue with an active Agent".to_owned(),
                        truncated: false,
                    },
                },
            ],
        )
        .expect("start an active Turn for Agent output");
    apply_next_session_event(&mut application, &mut subscription).await;

    let output = server.agent_output();
    output
        .emit(
            session_id,
            AgentOutput::Activity {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: "Reading files".to_owned(),
                },
            },
        )
        .expect("publish status Activity");
    apply_next_session_event(&mut application, &mut subscription).await;

    let message_id = MessageId::new();
    output
        .emit(
            session_id,
            AgentOutput::MessageStarted {
                message_id,
                turn_id,
            },
        )
        .expect("start Agent Message");
    apply_next_session_event(&mut application, &mut subscription).await;
    output
        .emit(
            session_id,
            AgentOutput::MessageDelta {
                message_id,
                content: "# Streamed heading\n\nA *useful* [link](https://example.com) with `inline code`.\n\n- first item\n- second item\n\n```rust\nfn main() {"
                    .to_owned(),
            },
        )
        .expect("publish first Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;

    let partial = rendered_application_buffer(&application, 100, 34);
    assert_eq!(
        buffer_rows(&partial)
            .iter()
            .filter(|row| row.contains("Streamed heading"))
            .count(),
        1,
        "one streamed Message must render once rather than once per chunk"
    );

    output
        .emit(
            session_id,
            AgentOutput::MessageDelta {
                message_id,
                content: "\n\n    println!(\"hi\");\n}\n```\n\n<future>Readable fallback</future>"
                    .to_owned(),
            },
        )
        .expect("publish final Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;
    output
        .emit(session_id, AgentOutput::MessageCompleted { message_id })
        .expect("complete Agent Message");
    apply_next_session_event(&mut application, &mut subscription).await;

    let completed = rendered_application_buffer(&application, 100, 34);
    let rows = buffer_rows(&completed);
    let screen = rows.join("\n");
    for readable in [
        "Streamed heading",
        "A useful link (https://example.com) with inline code.",
        "• first item",
        "• second item",
        "fn main() {",
        "println!(\"hi\");",
        "Readable fallback",
    ] {
        assert!(
            screen.contains(readable),
            "missing rendered Markdown: {readable}"
        );
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Streamed heading"))
            .count(),
        1,
        "completion must preserve the stable Message row"
    );

    let user_row = text_position(&completed, "Explain the stream").1;
    let error_row = text_position(&completed, "Error:").1;
    let status_row = text_position(&completed, "Reading files").1;
    let agent_row = text_position(&completed, "Streamed heading").1;
    let code_start_row = text_position(&completed, "fn main() {").1;
    let code_after_blank_row = text_position(&completed, "println!(\"hi\");").1;
    let user_accent_column = text_position(&completed, "Explain the stream")
        .0
        .saturating_sub(2);
    let user_block_right_edge = completed.area.width.saturating_sub(3);
    assert!(user_row < error_row && error_row < status_row && status_row < agent_row);
    assert_eq!(
        code_after_blank_row,
        code_start_row + 2,
        "fenced code preserves blank lines: {:?}",
        &rows[usize::from(code_start_row)..=usize::from(code_after_blank_row)]
    );
    let accented_user_rows = (user_row..error_row)
        .filter(|row| {
            completed
                .cell((user_accent_column, *row))
                .is_some_and(|cell| cell.symbol() == "┃")
        })
        .collect::<Vec<_>>();
    assert!(
        accented_user_rows.len() >= 3,
        "source and wrapped user lines keep the block accent"
    );
    for row in accented_user_rows {
        assert_eq!(
            completed
                .cell((user_accent_column, row))
                .expect("accent cell")
                .fg,
            Color::Cyan
        );
        assert_eq!(
            completed
                .cell((user_block_right_edge, row))
                .expect("elevated row edge")
                .bg,
            Color::Black,
            "the elevated surface spans the full user block width"
        );
    }
    assert_eq!(text_cell(&completed, "┃").fg, Color::Cyan);
    assert_eq!(text_cell(&completed, "Explain the stream").bg, Color::Black);
    assert_eq!(text_cell(&completed, "Error:").fg, Color::Red);
    assert_eq!(text_cell(&completed, "Reading files").fg, Color::DarkGray);
    assert_eq!(text_cell(&completed, "Streamed heading").fg, Color::Cyan);
    assert!(
        text_cell(&completed, "Streamed heading")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        text_cell(&completed, "useful")
            .modifier
            .contains(Modifier::ITALIC)
    );
    assert_eq!(text_cell(&completed, "link").fg, Color::Blue);
    assert!(
        text_cell(&completed, "link")
            .modifier
            .contains(Modifier::UNDERLINED)
    );
    assert_eq!(text_cell(&completed, "inline code").fg, Color::Yellow);

    drop(subscription);
    drop(client);
    drop(output);
    server.shutdown().await.expect("shut down server");
}

#[test]
fn command_activities_render_active_successful_and_failed_states_at_responsive_widths() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let cases = [
        (ActivityStatus::Active, None, "$ cargo test", Color::Cyan),
        (
            ActivityStatus::Completed,
            Some(0),
            "✓ cargo test",
            Color::Green,
        ),
        (
            ActivityStatus::Failed,
            Some(17),
            "× cargo test (exit 17)",
            Color::Red,
        ),
    ];

    for (status, exit_status, heading, color) in cases {
        let mut snapshot = failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Run the test suite",
            workspace.path(),
        );
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::Command {
            id: activity_id,
            turn_id: snapshot.turns[0].id,
            status,
            command: "cargo test".to_owned(),
            cwd: Some("/fixture/work".into()),
            output: "running tests\ntest result available\n".to_owned(),
            output_truncated: false,
            exit_status,
        };
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with command Activity");

        let desktop = rendered_application_buffer(&application, 100, 22);
        let desktop_text = buffer_rows(&desktop).join("\n");
        for expected in [
            heading,
            "in /fixture/work",
            "running tests",
            "test result available",
        ] {
            assert!(
                desktop_text.contains(expected),
                "desktop command Activity omitted {expected:?}:\n{desktop_text}"
            );
        }
        assert_eq!(text_cell(&desktop, heading).fg, color);

        let compact = rendered_application_rows_at(&application, 43, 18).join("\n");
        for expected in [
            heading,
            "in /fixture/work",
            "running tests",
            "test result available",
        ] {
            assert!(
                compact.contains(expected),
                "compact command Activity omitted {expected:?}:\n{compact}"
            );
        }
    }
}

#[test]
fn file_change_activities_render_active_successful_and_failed_states_at_responsive_widths() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let cases = [
        (
            ActivityStatus::Active,
            "… Applying file changes",
            Color::Cyan,
        ),
        (
            ActivityStatus::Completed,
            "✓ Applied file changes",
            Color::Green,
        ),
        (
            ActivityStatus::Failed,
            "× Failed to apply file changes",
            Color::Red,
        ),
    ];

    for (status, heading, color) in cases {
        let mut snapshot = failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Change these files",
            workspace.path(),
        );
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::FileChange {
            id: activity_id,
            turn_id: snapshot.turns[0].id,
            status,
            changes: vec![
                FileChange::Update {
                    path: "src/a.rs".into(),
                    moved_to: Some("src/b.rs".into()),
                },
                FileChange::Add {
                    path: "tests/new.rs".into(),
                },
                FileChange::Delete {
                    path: "old.rs".into(),
                },
            ],
        };
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with file-change Activity");

        let desktop = rendered_application_buffer(&application, 100, 22);
        let desktop_text = buffer_rows(&desktop).join("\n");
        for expected in [
            heading,
            "R src/a.rs → src/b.rs",
            "A tests/new.rs",
            "D old.rs",
        ] {
            assert!(
                desktop_text.contains(expected),
                "desktop file-change Activity omitted {expected:?}:\n{desktop_text}"
            );
        }
        assert_eq!(text_cell(&desktop, heading).fg, color);

        let compact = rendered_application_rows_at(&application, 43, 18).join("\n");
        for expected in [
            heading,
            "R src/a.rs → src/b.rs",
            "A tests/new.rs",
            "D old.rs",
        ] {
            assert!(
                compact.contains(expected),
                "compact file-change Activity omitted {expected:?}:\n{compact}"
            );
        }
    }
}

#[test]
fn streaming_command_updates_reuse_one_projected_transcript_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let mut snapshot = failed_session_snapshot(
        session_id,
        PromptId::new(),
        "Run the test suite",
        workspace.path(),
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let turn_id = snapshot.turns[0].id;
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Command {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Active,
        command: "cargo test".to_owned(),
        cwd: None,
        output: String::new(),
        output_truncated: false,
        exit_status: None,
    };
    let initial_revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach active command Activity");

    for (revision, content) in [
        (initial_revision.0 + 1, "running "),
        (initial_revision.0 + 2, "tests\n"),
    ] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                SessionUpdate {
                    session_id,
                    revision: SessionRevision(revision),
                    changes: vec![SessionChange::CommandOutputAppended {
                        activity_id,
                        content: content.to_owned(),
                    }],
                },
            )))
            .expect("project streamed command output");
    }
    let streamed = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert_eq!(streamed.matches("$ cargo test").count(), 1);
    assert_eq!(streamed.matches("running tests").count(), 1);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(initial_revision.0 + 3),
                changes: vec![SessionChange::CommandStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    exit_status: Some(0),
                }],
            },
        )))
        .expect("project command completion");
    let completed = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert_eq!(completed.matches("✓ cargo test").count(), 1);
    assert!(!completed.contains("$ cargo test"));
    assert_eq!(completed.matches("running tests").count(), 1);
}

#[test]
fn page_up_exposes_latest_and_end_resumes_following_the_transcript() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 8),
        ))
        .expect("attach a long Session");

    let latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(latest.contains("Agent section 8"));
    assert!(!latest.contains("Latest"));

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            )))
            .expect("page up through transcript content"),
        ApplicationTransition::Continue
    );
    let reading_history = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(reading_history.contains("Latest"));
    assert!(!reading_history.contains("Agent section 8"));

    for _ in 0..2 {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::PageDown,
                    KeyModifiers::NONE,
                )))
                .expect("page down through transcript content"),
            ApplicationTransition::Continue
        );
        rendered_application_rows_at(&application, 72, 18);
    }
    let paged_to_latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(paged_to_latest.contains("Agent section 8"));
    assert!(!paged_to_latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away again before using End");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Latest")
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::NONE,
            )))
            .expect("return to latest transcript content"),
        ApplicationTransition::Continue
    );
    let resumed = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(resumed.contains("Agent section 8"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn a_scrolled_message_anchor_survives_streaming_and_terminal_resize_per_client() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let mut snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    snapshot.session.status = SessionStatus::Active;
    snapshot
        .turns
        .last_mut()
        .expect("fixture has a final Turn")
        .status = TurnStatus::Active;
    let streaming_message_id = snapshot
        .messages
        .last_mut()
        .map(|message| {
            message.status = MessageStatus::Streaming;
            message.id
        })
        .expect("fixture has a final Agent Message");

    let mut reader = Application::new(workspace.path());
    reader
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach historical reader");
    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach tail-following observer");
    rendered_application_rows_at(&reader, 72, 18);
    rendered_application_rows_at(&observer, 72, 18);
    reader
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("reader pages into history");
    let anchored_rows = rendered_application_rows_at(&reader, 72, 18);
    let anchor_row = rendered_row(&anchored_rows, "Agent section 5");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 5"),
        "expected the fifth Agent Message to be the visible anchor:\n{anchored}"
    );

    let appended = SessionUpdate {
        session_id,
        revision: SessionRevision(snapshot.revision.0 + 1),
        changes: vec![SessionChange::MessageContentAppended {
            message_id: streaming_message_id,
            content: "\n\nSTREAMED TAIL that only a client following the bottom should see"
                .to_owned(),
        }],
    };
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                appended.clone(),
            )))
            .expect("append streamed Agent content");
    }

    let resized_reader_rows = rendered_application_rows_at(&reader, 42, 15);
    assert_eq!(
        rendered_row(&resized_reader_rows, "Agent section 5"),
        anchor_row,
        "resize must keep the Message on the same rendered row"
    );
    let resized_reader = resized_reader_rows.join("\n");
    assert!(
        resized_reader.contains("Agent section 5"),
        "resize must preserve the logical Message anchor:\n{resized_reader}"
    );
    assert!(resized_reader.contains("Latest"));
    assert!(!resized_reader.contains("STREAMED TAIL"));
    let following_observer = rendered_application_rows_at(&observer, 42, 15).join("\n");
    assert!(following_observer.contains("STREAMED TAIL"));
    assert!(!following_observer.contains("Latest"));

    let completed = SessionUpdate {
        session_id,
        revision: SessionRevision(appended.revision.0 + 1),
        changes: vec![
            SessionChange::MessageCompleted {
                message_id: streaming_message_id,
            },
            SessionChange::TurnStatusChanged {
                turn_id: snapshot.turns.last().expect("fixture has a final Turn").id,
                status: TurnStatus::Completed,
            },
            SessionChange::SessionStatusChanged {
                status: SessionStatus::Idle,
            },
        ],
    };
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                completed.clone(),
            )))
            .expect("complete streamed Agent content");
    }
    let completed_reader_rows = rendered_application_rows_at(&reader, 56, 20);
    assert_eq!(
        rendered_row(&completed_reader_rows, "Agent section 5"),
        anchor_row,
        "completion must keep the Message on the same rendered row"
    );
    let completed_reader = completed_reader_rows.join("\n");
    assert!(completed_reader.contains("Agent section 5"));
    assert!(completed_reader.contains("Latest"));
    assert!(
        rendered_application_rows_at(&observer, 56, 20)
            .join("\n")
            .contains("STREAMED TAIL")
    );
}

#[test]
fn resize_that_reveals_the_whole_transcript_resumes_following() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            navigable_session_snapshot(SessionId::new(), workspace.path(), 4),
        ))
        .expect("attach a long Session");
    rendered_application_rows_at(&application, 40, 12);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into transcript history");
    assert!(
        rendered_application_rows_at(&application, 40, 12)
            .join("\n")
            .contains("Latest")
    );

    let expanded = rendered_application_rows_at(&application, 100, 40).join("\n");
    assert!(expanded.contains("Agent section 4"));
    assert!(!expanded.contains("Latest"));

    let compact_again = rendered_application_rows_at(&application, 40, 12).join("\n");
    assert!(compact_again.contains("Agent section 4"));
    assert!(!compact_again.contains("Latest"));
}

#[test]
fn transcript_navigation_remains_correct_beyond_the_terminal_scroll_limit() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let prompt_id = PromptId::new();
    let content = format!("{}TAIL beyond u16", "x\n".repeat(65_700));
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            SessionId::new(),
            prompt_id,
            &content,
            workspace.path(),
        )))
        .expect("attach a transcript longer than Ratatui's local scroll offset");

    let latest = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(latest.contains("TAIL beyond u16"));
    assert!(!latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away from a very long tail");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Latest")
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )))
        .expect("return to the very long tail");
    let resumed = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(resumed.contains("TAIL beyond u16"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn transcript_navigation_reaches_tail_of_one_oversized_wrapped_line() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let terminal_width = 28;
    let transcript_width = terminal_width - 2;
    let agent_message = snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Agent)
        .expect("fixture has an Agent Message");
    agent_message.content = format!(
        "{} TAIL",
        "x".repeat(usize::from(transcript_width) * 65_700)
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach one Agent line longer than Ratatui's local scroll offset");

    let latest = rendered_application_rows_at(&application, terminal_width, 18).join("\n");
    assert!(latest.contains("TAIL"));
    assert!(!latest.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page away from the oversized wrapped line tail");
    assert!(
        rendered_application_rows_at(&application, terminal_width, 18)
            .join("\n")
            .contains("Latest")
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )))
        .expect("return to the oversized wrapped line tail");
    let resumed = rendered_application_rows_at(&application, terminal_width, 18).join("\n");
    assert!(resumed.contains("TAIL"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn message_anchor_survives_prompt_reconciliation_and_composer_dock_layout_changes() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach a long Session");
    rendered_application_rows_at(&application, 72, 22);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into transcript history");
    let anchored_rows = rendered_application_rows_at(&application, 72, 22);
    let anchor_row = rendered_row(&anchored_rows, "Agent section 4");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 4"),
        "expected the fourth Agent Message to be the visible anchor:\n{anchored}"
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Reconciled line one\nline two\nline three\nline four\nline five\nfinal draft row"
                .to_owned(),
        )))
        .expect("grow the multiline composer");
    let growing_composer_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&growing_composer_rows, "Agent section 4"),
        anchor_row,
        "composer growth must keep the Message on the same rendered row"
    );
    let growing_composer = growing_composer_rows.join("\n");
    assert!(growing_composer.contains("Agent section 4"));
    assert!(growing_composer.contains("final draft row"));
    assert!(growing_composer.contains("Latest"));

    let queued_prompt_id = PromptId::new();
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: queued_prompt_id,
                        text: "Queued dock entry".to_owned(),
                        delivery: PromptDelivery::Queue,
                        admission_order: PromptOrder(9),
                        status: PromptStatus::Pending,
                    },
                }],
            },
        )))
        .expect("show a queued Prompt dock");
    let with_dock_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&with_dock_rows, "Agent section 4"),
        anchor_row,
        "queued dock changes must keep the Message on the same rendered row"
    );
    let with_dock = with_dock_rows.join("\n");
    assert!(with_dock.contains("Agent section 4"));
    assert!(with_dock.contains("Queued dock entry"));
    assert!(with_dock.contains("Latest"));

    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the multiline steer optimistically")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    let provisional_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&provisional_rows, "Agent section 4"),
        anchor_row,
        "provisional content must keep the Message on the same rendered row"
    );
    let provisional = provisional_rows.join("\n");
    assert!(provisional.contains("Agent section 4"));
    assert!(provisional.contains("Latest"));

    let delivered_turn_id = TurnId::new();
    let delivered_message_id = MessageId::new();
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 2),
                changes: vec![
                    SessionChange::PromptAdded {
                        prompt: Prompt {
                            id: request.prompt.id,
                            text: request.prompt.text.clone(),
                            delivery: PromptDelivery::Steer,
                            admission_order: PromptOrder(10),
                            status: PromptStatus::Delivered,
                        },
                    },
                    SessionChange::TurnAdded {
                        turn: Turn {
                            id: delivered_turn_id,
                            prompt_id: request.prompt.id,
                            agent: None,
                            status: TurnStatus::Active,
                        },
                    },
                    SessionChange::MessageAdded {
                        message: Message {
                            id: delivered_message_id,
                            turn_id: delivered_turn_id,
                            role: MessageRole::User,
                            status: MessageStatus::Completed,
                            content: request.prompt.text.clone(),
                            truncated: false,
                        },
                    },
                ],
            },
        )))
        .expect("reconcile the optimistic Prompt to a stable Message");
    let reconciled_anchor_rows = rendered_application_rows_at(&application, 72, 22);
    assert_eq!(
        rendered_row(&reconciled_anchor_rows, "Agent section 4"),
        anchor_row,
        "Message reconciliation must keep the anchor on the same rendered row"
    );
    let reconciled_anchor = reconciled_anchor_rows.join("\n");
    assert!(reconciled_anchor.contains("Agent section 4"));
    assert!(reconciled_anchor.contains("Latest"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("start the queued-Prompt leader");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .expect("open queued-Prompt mode");
    for code in [KeyCode::PageUp, KeyCode::PageDown] {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE,)))
                .expect("scoped mode owns transcript navigation keys"),
            ApplicationTransition::Continue
        );
    }
    let ApplicationTransition::PromotePrompt { prompt_id, .. } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("queued-Prompt selection remains active")
    else {
        panic!("Page keys must not escape queued-Prompt mode");
    };
    assert_eq!(prompt_id, queued_prompt_id);

    application
        .handle_event(ApplicationEvent::Command(CommandId::FollowLatest))
        .expect("return to the reconciled tail");
    let latest = rendered_application_rows_at(&application, 72, 22).join("\n");
    assert_eq!(latest.matches("Reconciled line one").count(), 1);
    assert!(!latest.contains("Latest"));
}

/// A Session whose single Activity is a command carrying `output`, so a test
/// can drive one entry's Fold without competing transcript content.
fn command_activity_session(
    workspace: &std::path::Path,
    status: ActivityStatus,
    output: &str,
    output_truncated: bool,
) -> (suru::protocol::SessionSnapshot, ActivityId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Run the test suite",
        workspace,
    );
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Command {
        id: activity_id,
        turn_id: snapshot.turns[0].id,
        status,
        command: "cargo test".to_owned(),
        cwd: None,
        output: output.to_owned(),
        output_truncated,
        exit_status: match status {
            ActivityStatus::Active => None,
            ActivityStatus::Completed | ActivityStatus::Failed => Some(0),
        },
    };
    (snapshot, activity_id)
}

fn numbered_output(lines: usize) -> String {
    prefixed_output("output", lines)
}

fn left_click_at(row: u16) -> InputEvent {
    InputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 6,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn settled_command_output_folds_to_a_head_and_tail_around_a_fold_marker() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long command Activity");

    let folded = rendered_application_rows_at(&application, 60, 24).join("\n");

    for head in ["output line 1", "output line 2", "output line 3"] {
        assert!(
            folded.contains(head),
            "a Fold keeps the head of the output: {folded}"
        );
    }
    for tail in ["output line 10", "output line 11", "output line 12"] {
        assert!(
            folded.contains(tail),
            "a Fold keeps the tail of the output: {folded}"
        );
    }
    for hidden in ["output line 5", "output line 6", "output line 7"] {
        assert!(
            !folded.contains(hidden),
            "a Fold hides the middle of the output: {folded}"
        );
    }
    assert!(
        folded.contains("… +6 lines"),
        "a folded entry says how much it hides: {folded}"
    );
}

#[test]
fn the_fold_marker_counts_logical_lines_so_it_reads_the_same_at_every_width() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long command Activity");

    let narrow = rendered_application_rows_at(&application, 44, 24).join("\n");
    let wide = rendered_application_rows_at(&application, 110, 24).join("\n");

    assert!(narrow.contains("… +6 lines"), "narrow frame: {narrow}");
    assert!(wide.contains("… +6 lines"), "wide frame: {wide}");
}

#[test]
fn long_output_lines_wrap_before_the_clamp_so_a_few_cannot_flood_the_fold() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let output = (1..=4)
        .map(|line| format!("line {line} {}", "x".repeat(200)))
        .collect::<Vec<_>>()
        .join("\n");
    let (snapshot, _) =
        command_activity_session(workspace.path(), ActivityStatus::Completed, &output, false);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with very long output lines");

    let rows = rendered_application_rows_at(&application, 60, 24);
    let folded = rows.join("\n");

    assert!(
        folded.contains("… +4 lines"),
        "the marker counts the four source lines it replaced, not their wrapped rows: {folded}"
    );
    assert!(
        !folded.contains("xxxx"),
        "no wrapped row of a clamped line survives the Fold: {folded}"
    );
}

#[test]
fn clicking_a_folded_command_expands_it_and_clicking_its_header_folds_it_back() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long command Activity");
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    let marker_row = rendered_row(&folded_rows, "… +6 lines") as u16;

    assert_eq!(
        application
            .handle_terminal_event(left_click_at(marker_row))
            .expect("click the fold marker"),
        ApplicationTransition::Continue
    );
    let expanded_rows = rendered_application_rows_at(&application, 60, 24);
    let expanded = expanded_rows.join("\n");
    for line in 1..=12 {
        assert!(
            expanded.contains(&format!("output line {line}")),
            "expanding reveals everything stored: {expanded}"
        );
    }
    assert!(
        !expanded.contains("… +"),
        "no fold marker remains: {expanded}"
    );

    let header_row = rendered_row(&expanded_rows, "✓ cargo test") as u16;
    application
        .handle_terminal_event(left_click_at(header_row))
        .expect("click the entry header");
    let refolded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        refolded.contains("… +6 lines"),
        "clicking the header folds the entry again: {refolded}"
    );
}

#[test]
fn clicking_inside_expanded_output_leaves_the_entry_expanded() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long command Activity");
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&folded_rows, "… +6 lines") as u16
        ))
        .expect("expand the entry");
    let expanded_rows = rendered_application_rows_at(&application, 60, 24);

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&expanded_rows, "output line 6") as u16
        ))
        .expect("click inside the revealed output");

    let after = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        after.contains("output line 6"),
        "a click on output never folds away the content under the pointer: {after}"
    );
    assert!(!after.contains("… +"), "the entry stays expanded: {after}");
}

#[test]
fn toggling_the_fold_posture_expands_every_entry_and_clears_per_entry_overrides() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long command Activity");
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&folded_rows, "… +6 lines") as u16
        ))
        .expect("expand one entry by hand");

    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("invoke transcript.folds.toggle");
    }
    let expanded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        expanded.contains("output line 6") && !expanded.contains("… +"),
        "the expanded posture shows every entry in full: {expanded}"
    );

    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("invoke transcript.folds.toggle again");
    }
    let refolded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        refolded.contains("… +6 lines"),
        "flipping back folds the entry the reader had expanded by hand: {refolded}"
    );
}

#[test]
fn error_and_status_activities_are_never_folded() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Run the test suite",
        workspace.path(),
    );
    let turn_id = snapshot.turns[0].id;
    let status_id = ActivityId::new();
    snapshot.activities[0] = Activity::Error {
        id: snapshot.activities[0].id(),
        turn_id,
        text: (1..=10)
            .map(|line| format!("failure detail {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    snapshot.activities.push(Activity::Status {
        id: status_id,
        turn_id,
        text: (1..=10)
            .map(|line| format!("status detail {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: status_id,
    });
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long Error and Status");

    let rendered = rendered_application_rows_at(&application, 60, 40).join("\n");

    for line in 1..=10 {
        assert!(
            rendered.contains(&format!("failure detail {line}")),
            "a failure the reader cannot see is the one thing a Fold must never hide: {rendered}"
        );
        assert!(
            rendered.contains(&format!("status detail {line}")),
            "Status Activities are not folded: {rendered}"
        );
    }
    assert!(
        !rendered.contains("… +"),
        "no fold marker appears: {rendered}"
    );
}

#[test]
fn file_change_activities_fold_past_the_path_budget_and_expand_on_click() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Change these files",
        workspace.path(),
    );
    snapshot.activities[0] = Activity::FileChange {
        id: snapshot.activities[0].id(),
        turn_id: snapshot.turns[0].id,
        status: ActivityStatus::Completed,
        changes: (1..=7)
            .map(|change| FileChange::Add {
                path: format!("src/file{change}.rs").into(),
            })
            .collect(),
    };
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with many file changes");

    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    let folded = folded_rows.join("\n");
    assert!(
        folded.contains("A src/file4.rs"),
        "the budget lists four paths: {folded}"
    );
    assert!(
        !folded.contains("A src/file5.rs"),
        "paths past the budget are folded away: {folded}"
    );
    assert!(
        folded.contains("… +3 more"),
        "the fold marker counts the paths it hides: {folded}"
    );

    application
        .handle_terminal_event(left_click_at(rendered_row(&folded_rows, "… +3 more") as u16))
        .expect("expand the file-change entry");
    let expanded = rendered_application_rows_at(&application, 60, 24).join("\n");
    for change in 1..=7 {
        assert!(
            expanded.contains(&format!("A src/file{change}.rs")),
            "expanding lists every stored path: {expanded}"
        );
    }
}

/// A Session whose single Activity is a Reasoning block, so a test can drive
/// one entry's Fold without competing transcript content.
fn reasoning_activity_session(
    workspace: &std::path::Path,
    status: ActivityStatus,
    title: Option<&str>,
    content: &str,
    duration_ms: Option<u64>,
) -> (suru::protocol::SessionSnapshot, ActivityId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Explain the Transcript",
        workspace,
    );
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Reasoning {
        id: activity_id,
        turn_id: snapshot.turns[0].id,
        status,
        title: title.map(ToOwned::to_owned),
        content: content.to_owned(),
        content_truncated: false,
        duration_ms,
    };
    (snapshot, activity_id)
}

#[test]
fn folded_reasoning_is_one_line_naming_its_title_and_how_long_it_took() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        Some("Inspecting the seam"),
        "Reading the projection.\n\nThen the store.",
        Some(72_000),
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a settled Reasoning Activity");

    let folded_rows = rendered_application_rows_at(&application, 72, 24);
    let folded = folded_rows.join("\n");
    assert_eq!(
        folded_rows[rendered_row(&folded_rows, "Thought: Inspecting the seam")].trim_end(),
        "    ✓ Thought: Inspecting the seam · 1m 12s · +3 lines",
        "a folded Reasoning block is one line naming its title, its duration, \
         and how much its Fold hides"
    );
    assert!(
        !folded.contains("Reading the projection"),
        "a folded Reasoning block hides the summary itself: {folded}"
    );

    application
        .handle_terminal_event(left_click_at(rendered_row(
            &folded_rows,
            "Thought: Inspecting the seam",
        ) as u16))
        .expect("expand the Reasoning entry");
    let expanded_rows = rendered_application_rows_at(&application, 72, 24);
    let expanded = expanded_rows.join("\n");
    for revealed in ["Reading the projection.", "Then the store."] {
        assert!(
            expanded.contains(revealed),
            "expanding a Reasoning block reveals everything stored: {expanded}"
        );
    }
    assert_eq!(
        expanded_rows[rendered_row(&expanded_rows, "Thought: Inspecting the seam")].trim_end(),
        "    ✓ Thought: Inspecting the seam · 1m 12s",
        "an expanded Reasoning block keeps the header that re-folds it, and drops \
         the count of what it is no longer hiding"
    );
}

#[test]
fn reasoning_still_running_heads_with_its_running_label_and_no_duration() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        Some("Inspecting the seam"),
        "Reading the projection.",
        None,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a streaming Reasoning Activity");

    let streaming_rows = rendered_application_rows_at(&application, 72, 24);
    let streaming = streaming_rows.join("\n");

    let header = &streaming_rows[rendered_row(&streaming_rows, "Thinking: Inspecting the seam")];
    assert_eq!(
        header.trim_end(),
        "    … Thinking: Inspecting the seam · +1 lines",
        "a Reasoning block still running heads with its running label and no duration"
    );
    assert!(
        !streaming.contains("Reading the projection."),
        "a Reasoning block still running is folded like any other: {streaming}"
    );
}

#[test]
fn untitled_reasoning_heads_with_the_label_alone() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        None,
        "Reading the projection.",
        Some(4_200),
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an untitled Reasoning Activity");

    let folded = rendered_application_rows_at(&application, 72, 24).join("\n");

    assert!(
        folded.contains("Thought · 4s · +1 lines"),
        "a Reasoning block the Provider never titled still heads its Fold: {folded}"
    );
}

#[test]
fn an_active_command_shows_a_live_tail_and_settles_into_a_head_and_tail_fold() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        &numbered_output(12),
        false,
    );
    let session_id = snapshot.session.id;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a streaming command");

    let streaming = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        streaming.contains("output line 12") && streaming.contains("output line 10"),
        "a streaming command shows the tail it is writing now: {streaming}"
    );
    assert!(
        !streaming.contains("output line 3") && !streaming.contains("output line 9"),
        "a live tail keeps no head: {streaming}"
    );
    assert!(
        streaming.contains("… +9 lines"),
        "the live tail says how much it hides: {streaming}"
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    exit_status: Some(0),
                }],
            },
        )))
        .expect("settle the command");

    let settled = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        settled.contains("output line 1") && settled.contains("output line 12"),
        "a settled command takes the head-and-tail form: {settled}"
    );
    assert!(
        settled.contains("… +6 lines"),
        "settled fold marker: {settled}"
    );
}

#[test]
fn interrupting_a_turn_expands_the_activity_the_reader_was_watching() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        &numbered_output(12),
        false,
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a streaming command");
    assert!(
        rendered_application_rows_at(&application, 60, 24)
            .join("\n")
            .contains("… +9 lines")
    );

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }

    let interrupted = rendered_application_rows_at(&application, 60, 40).join("\n");
    for line in 1..=12 {
        assert!(
            interrupted.contains(&format!("output line {line}")),
            "the Activity the reader was watching stays visible after an interrupt: {interrupted}"
        );
    }
}

#[test]
fn expanding_a_capped_command_reveals_everything_stored_before_the_truncation_marker() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        true,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with capped command output");

    let folded_rows = rendered_application_rows_at(&application, 60, 30);
    let folded = folded_rows.join("\n");
    assert!(
        folded.contains("… +6 lines") && folded.contains("[output truncated]"),
        "one entry carries both a Fold and a Truncation: {folded}"
    );

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&folded_rows, "… +6 lines") as u16
        ))
        .expect("expand the capped entry");

    let expanded_rows = rendered_application_rows_at(&application, 60, 30);
    let expanded = expanded_rows.join("\n");
    for line in 1..=12 {
        assert!(
            expanded.contains(&format!("output line {line}")),
            "expanding a Fold reveals everything stored: {expanded}"
        );
    }
    assert!(
        !expanded.contains("… +"),
        "the fold marker is gone: {expanded}"
    );
    assert!(
        rendered_row(&expanded_rows, "[output truncated]")
            > rendered_row(&expanded_rows, "output line 12"),
        "the expanded entry still ends at the truncation marker: {expanded}"
    );
}

#[test]
fn fold_state_stays_local_to_the_client_that_flipped_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut reader = connected_application(workspace.path());
    let mut observer = connected_application(workspace.path());
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .expect("attach both clients to the same Session");
    }
    let folded_rows = rendered_application_rows_at(&reader, 60, 24);

    reader
        .handle_terminal_event(left_click_at(
            rendered_row(&folded_rows, "… +6 lines") as u16
        ))
        .expect("one client expands the entry");

    assert!(
        !rendered_application_rows_at(&reader, 60, 24)
            .join("\n")
            .contains("… +"),
        "the client that expanded sees the whole entry"
    );
    let observed = rendered_application_rows_at(&observer, 60, 24).join("\n");
    assert!(
        observed.contains("… +6 lines"),
        "a Fold is client-local view state and never reaches another client: {observed}"
    );
}

#[test]
fn clicking_an_entry_that_hides_nothing_records_no_fold_for_its_later_output() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        &numbered_output(2),
        false,
    );
    let session_id = snapshot.session.id;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose command has hidden nothing yet");
    let rows = rendered_application_rows_at(&application, 60, 24);
    assert!(
        !rows.join("\n").contains("… +"),
        "the fixture starts with nothing folded"
    );

    application
        .handle_terminal_event(left_click_at(rendered_row(&rows, "$ cargo test") as u16))
        .expect("click an entry that hides nothing");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: format!("\n{}", numbered_output(12)),
                }],
            },
        )))
        .expect("stream more output than the Fold budget");

    let grown = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        grown.contains("… +"),
        "a click on an entry with nothing to hide leaves the posture in charge: {grown}"
    );
}

#[test]
fn clicks_do_not_reach_the_transcript_while_a_picker_covers_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a folded command Activity");
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    let marker_row = rendered_row(&folded_rows, "… +6 lines") as u16;

    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("open the Session picker over the transcript");
    }
    assert_eq!(
        application.command_for_terminal_input(left_click_at(marker_row)),
        None,
        "a picker owns the surface, so a click never reaches the transcript beneath it"
    );
    application
        .handle_terminal_event(left_click_at(marker_row))
        .expect("click while the picker covers the transcript");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close the Session picker");

    let after = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        after.contains("… +6 lines"),
        "the entry beneath the picker keeps its Fold: {after}"
    );
}

/// One transcript entry in a Group scenario, so a test states the shape of the
/// run it drives instead of assembling Activities by hand. Commands are
/// numbered `command 1`, `command 2`, … in transcript order so member rows are
/// assertable by name.
enum RunEntry {
    Command(ActivityStatus, Option<i32>),
    AgentMessage(&'static str),
    UserMessage(&'static str),
    Reasoning(&'static str),
    FileChange,
    Status(&'static str),
    Error(&'static str),
}

/// A Session whose transcript is exactly `entries` under one Turn.
fn command_run_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
    entries: &[RunEntry],
) -> suru::protocol::SessionSnapshot {
    let mut snapshot =
        failed_session_snapshot(session_id, PromptId::new(), "Run the workflow", workspace);
    let turn_id = snapshot.turns[0].id;
    snapshot.messages.clear();
    snapshot.activities.clear();
    snapshot.transcript.clear();
    let mut command_number = 0;
    for entry in entries {
        let activity = match entry {
            RunEntry::AgentMessage(content) | RunEntry::UserMessage(content) => {
                let message = Message {
                    id: MessageId::new(),
                    turn_id,
                    role: match entry {
                        RunEntry::UserMessage(_) => MessageRole::User,
                        _ => MessageRole::Agent,
                    },
                    status: MessageStatus::Completed,
                    content: (*content).to_owned(),
                    truncated: false,
                };
                snapshot.transcript.push(TranscriptItem::Message {
                    message_id: message.id,
                });
                snapshot.messages.push(message);
                continue;
            }
            RunEntry::Command(status, exit_status) => {
                command_number += 1;
                Activity::Command {
                    id: ActivityId::new(),
                    turn_id,
                    status: *status,
                    command: format!("command {command_number}"),
                    cwd: None,
                    output: format!("output of command {command_number}"),
                    output_truncated: false,
                    exit_status: *exit_status,
                }
            }
            RunEntry::Reasoning(title) => Activity::Reasoning {
                id: ActivityId::new(),
                turn_id,
                status: ActivityStatus::Completed,
                title: Some((*title).to_owned()),
                content: "Weighed the options.".to_owned(),
                content_truncated: false,
                duration_ms: None,
            },
            RunEntry::FileChange => Activity::FileChange {
                id: ActivityId::new(),
                turn_id,
                status: ActivityStatus::Completed,
                changes: vec![FileChange::Add {
                    path: "src/new.rs".into(),
                }],
            },
            RunEntry::Status(text) => Activity::Status {
                id: ActivityId::new(),
                turn_id,
                text: (*text).to_owned(),
            },
            RunEntry::Error(text) => Activity::Error {
                id: ActivityId::new(),
                turn_id,
                text: (*text).to_owned(),
            },
        };
        snapshot.transcript.push(TranscriptItem::Activity {
            activity_id: activity.id(),
        });
        snapshot.activities.push(activity);
    }
    snapshot
}

const SUCCESSFUL_COMMAND: RunEntry = RunEntry::Command(ActivityStatus::Completed, Some(0));

/// The shape a live run has mid-Turn: two settled successful commands then one
/// Active command, inside a Session and Turn still marked Active.
fn live_command_run_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
) -> suru::protocol::SessionSnapshot {
    let mut snapshot = command_run_snapshot(
        session_id,
        workspace,
        &[
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Command(ActivityStatus::Active, None),
        ],
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    snapshot
}

/// The Session update that settles a running command into `status`.
fn command_settles(
    session_id: SessionId,
    revision: SessionRevision,
    activity_id: ActivityId,
    status: ActivityStatus,
    exit_status: Option<i32>,
) -> ApplicationEvent {
    ApplicationEvent::Session(SessionEvent::Updated(SessionUpdate {
        session_id,
        revision,
        changes: vec![SessionChange::CommandStatusChanged {
            activity_id,
            status,
            exit_status,
        }],
    }))
}

#[test]
fn a_run_of_successful_commands_collapses_to_one_group_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of successful commands");

    let buffer = rendered_application_buffer(&application, 80, 18);
    let rendered = buffer_rows(&buffer).join("\n");

    assert!(
        rendered.contains("✓ Ran 3 commands"),
        "a run of three successful commands is one Group row: {rendered}"
    );
    for member in ["command 1", "command 2", "command 3", "output of command 1"] {
        assert!(
            !rendered.contains(member),
            "a collapsed Group hides its member rows, but {member:?} rendered: {rendered}"
        );
    }
    let affordance = text_cell(&buffer, "Ran 3 commands");
    assert_eq!(
        affordance.fg,
        Color::Blue,
        "the count is the expand affordance, styled action-primary"
    );
    assert!(
        affordance.modifier.contains(Modifier::BOLD),
        "action-primary carries its bold weight"
    );
    assert_eq!(
        text_cell(&buffer, "✓").fg,
        Color::Green,
        "the header keeps the settled Activity-header marker"
    );
    let rows = buffer_rows(&buffer);
    let header = &rows[rendered_row(&rows, "Ran 3 commands")];
    assert!(
        header.contains("  ✓ Ran 3 commands"),
        "the Group header sits in the Activity-header gutter: {header:?}"
    );
}

#[test]
fn a_run_of_one_successful_command_renders_as_a_normal_command_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &[SUCCESSFUL_COMMAND]);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with one successful command");

    let rendered = rendered_application_rows_at(&application, 80, 18).join("\n");

    assert!(
        rendered.contains("✓ command 1"),
        "a run of one renders the ordinary command row: {rendered}"
    );
    assert!(
        rendered.contains("output of command 1"),
        "the ordinary row keeps its output: {rendered}"
    );
    assert!(
        !rendered.contains("Ran 1 command"),
        "grouping never adds a layer where it saves nothing: {rendered}"
    );
}

#[test]
fn every_other_entry_kind_and_unsuccessful_commands_break_a_command_run() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let breakers: [(RunEntry, &str); 7] = [
        (
            RunEntry::AgentMessage("A breaking message"),
            "A breaking message",
        ),
        (
            RunEntry::UserMessage("A breaking user message"),
            "A breaking user message",
        ),
        (
            RunEntry::Reasoning("Weighing options"),
            "Thought: Weighing options",
        ),
        (RunEntry::FileChange, "✓ Applied file changes"),
        (
            RunEntry::Status("Agent Selection changed"),
            "Agent Selection changed",
        ),
        (RunEntry::Error("Provider failed"), "Error: Provider failed"),
        (
            RunEntry::Command(ActivityStatus::Failed, Some(2)),
            "× command 3 (exit 2)",
        ),
    ];
    for (breaker, visible) in breakers {
        let snapshot = command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[
                SUCCESSFUL_COMMAND,
                SUCCESSFUL_COMMAND,
                breaker,
                SUCCESSFUL_COMMAND,
                SUCCESSFUL_COMMAND,
            ],
        );
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach a Session with a broken command run");

        let rows = rendered_application_rows_at(&application, 80, 24);
        let rendered = rows.join("\n");
        assert_eq!(
            rendered.matches("Ran 2 commands").count(),
            2,
            "{visible:?} splits the run into two Groups: {rendered}"
        );
        assert!(
            rendered.contains(visible),
            "the breaking entry renders as its own row: {rendered}"
        );
        let first_group = rendered_row(&rows, "Ran 2 commands");
        let breaker_row = rendered_row(&rows, visible);
        assert!(
            first_group < breaker_row,
            "presentation order is preserved: {rendered}"
        );
        assert!(
            rows[breaker_row + 1..]
                .iter()
                .any(|row| row.contains("Ran 2 commands")),
            "the second Group renders after the breaking entry: {rendered}"
        );
    }
}

#[test]
fn an_active_command_renders_live_outside_the_group_while_the_turn_runs() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = live_command_run_snapshot(SessionId::new(), workspace.path());
    if let Activity::Command { output, .. } = &mut snapshot.activities[2] {
        *output = numbered_output(12);
    }
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");

    let rendered = rendered_application_rows_at(&application, 80, 18).join("\n");

    assert!(
        rendered.contains("✓ Ran 2 commands"),
        "the settled run groups while the Turn is still active: {rendered}"
    );
    assert!(
        !rendered.contains("command 1") && !rendered.contains("command 2"),
        "no member row escapes the Group: {rendered}"
    );
    assert!(
        rendered.contains("$ command 3"),
        "the running command stays visible outside the Group: {rendered}"
    );
    assert!(
        rendered.contains("output line 12") && rendered.contains("… +9 lines"),
        "the running command keeps its live-tail presentation: {rendered}"
    );
    assert!(
        !rendered.contains("output line 3"),
        "a live tail keeps no head: {rendered}"
    );
}

#[test]
fn groups_have_no_size_cap() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let entries = std::iter::repeat_with(|| SUCCESSFUL_COMMAND)
        .take(40)
        .collect::<Vec<_>>();
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &entries);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long run of successful commands");

    let rendered = rendered_application_rows_at(&application, 80, 18).join("\n");

    assert!(
        rendered.contains("✓ Ran 40 commands"),
        "a forty-command run is one row, not several Groups: {rendered}"
    );
    assert!(
        !rendered.contains("command 1") && !rendered.contains("command 40"),
        "no member row escapes the Group: {rendered}"
    );
}

#[test]
fn a_command_settling_successfully_is_absorbed_into_the_group_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let snapshot = live_command_run_snapshot(session_id, workspace.path());
    let running_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");

    let before = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert!(before.contains("✓ Ran 2 commands"), "{before}");

    application
        .handle_event(command_settles(
            session_id,
            next_revision,
            running_id,
            ActivityStatus::Completed,
            Some(0),
        ))
        .expect("project the command settling successfully");

    let after = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert!(
        after.contains("✓ Ran 3 commands"),
        "the Group row updates when a member settles into it: {after}"
    );
    assert!(
        !after.contains("Ran 2 commands") && !after.contains("command 3"),
        "the settled command left no standalone row behind: {after}"
    );
}

#[test]
fn a_command_settling_failed_stays_a_standalone_row_and_leaves_the_group_unchanged() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let snapshot = live_command_run_snapshot(session_id, workspace.path());
    let running_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");

    application
        .handle_event(command_settles(
            session_id,
            next_revision,
            running_id,
            ActivityStatus::Failed,
            Some(1),
        ))
        .expect("project the command settling failed");

    let rendered = rendered_application_rows_at(&application, 80, 18).join("\n");
    assert!(
        rendered.contains("✓ Ran 2 commands") && !rendered.contains("Ran 3 commands"),
        "a failed settle never joins the Group: {rendered}"
    );
    assert!(
        rendered.contains("× command 3 (exit 1)"),
        "the failed command stands alone as its own row: {rendered}"
    );
}

#[test]
fn an_interrupted_command_settles_as_a_standalone_failed_row_and_stays_expanded() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let mut snapshot = live_command_run_snapshot(session_id, workspace.path());
    if let Activity::Command { output, .. } = &mut snapshot.activities[2] {
        *output = numbered_output(12);
    }
    let running_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
    application
        .handle_event(command_settles(
            session_id,
            next_revision,
            running_id,
            ActivityStatus::Failed,
            None,
        ))
        .expect("project the interrupted command settling failed");

    let rendered = rendered_application_rows_at(&application, 80, 40).join("\n");
    assert!(
        rendered.contains("✓ Ran 2 commands") && !rendered.contains("Ran 3 commands"),
        "the Group is unchanged by the interrupt: {rendered}"
    );
    assert!(
        rendered.contains("× command 3") && !rendered.contains("(exit"),
        "an interrupt settles the row failed with no exit status to report: {rendered}"
    );
    for line in 1..=12 {
        assert!(
            rendered.contains(&format!("output line {line}")),
            "interrupt auto-expand keeps the watched output visible after the settle: {rendered}"
        );
    }
}

/// Rewrites the output of the run member holding `command`, so a test can give
/// one member more output than its Fold budget without touching the others.
fn set_member_output(
    snapshot: &mut suru::protocol::SessionSnapshot,
    command: &str,
    output: String,
) {
    let member = snapshot
        .activities
        .iter_mut()
        .find_map(|activity| match activity {
            Activity::Command {
                command: name,
                output: slot,
                ..
            } if name == command => Some(slot),
            _ => None,
        })
        .expect("the run holds the named command");
    *member = output;
}

fn prefixed_output(prefix: &str, lines: usize) -> String {
    (1..=lines)
        .map(|line| format!("{prefix} line {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn clicking_a_collapsed_group_expands_it_into_indented_folded_members() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    set_member_output(&mut snapshot, "command 2", numbered_output(12));
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of successful commands");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 30);

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
        ))
        .expect("click the collapsed Group row");

    let rows = rendered_application_rows_at(&application, 80, 30);
    let rendered = rows.join("\n");
    assert!(
        rendered.contains("✓ Ran 3 commands"),
        "the header stays when the Group expands: {rendered}"
    );
    for member in ["✓ command 1", "✓ command 2", "✓ command 3"] {
        assert!(
            rows.iter()
                .any(|row| row.starts_with(&format!("      {member}"))),
            "members render indented one gutter past a standalone row, missing {member:?}: {rendered}"
        );
    }
    let header = rendered_row(&rows, "Ran 3 commands");
    assert!(
        rendered_row(&rows, "✓ command 1") > header,
        "members render beneath the header: {rendered}"
    );
    for kept in [
        "output line 1",
        "output line 3",
        "output line 10",
        "output line 12",
    ] {
        assert!(
            rendered.contains(kept),
            "a member keeps its default Fold's head and tail: {rendered}"
        );
    }
    assert!(
        rendered.contains("… +6 lines") && !rendered.contains("output line 6"),
        "a member renders in its default Fold presentation, not in full: {rendered}"
    );
}

#[test]
fn a_members_fold_toggles_independently_within_an_expanded_group() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    set_member_output(&mut snapshot, "command 1", prefixed_output("first", 12));
    set_member_output(&mut snapshot, "command 2", prefixed_output("second", 12));
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of successful commands");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 36);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
        ))
        .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 36);
    assert_eq!(
        expanded_rows.join("\n").matches("… +6 lines").count(),
        2,
        "both members start out folded"
    );

    let first_marker = expanded_rows
        .iter()
        .position(|row| row.contains("… +6 lines"))
        .expect("the first member shows a fold marker");
    application
        .handle_terminal_event(left_click_at(first_marker as u16))
        .expect("expand one member's Fold");

    let rows = rendered_application_rows_at(&application, 80, 36);
    let rendered = rows.join("\n");
    for line in 1..=12 {
        assert!(
            rendered.contains(&format!("first line {line}")),
            "the clicked member expands in full: {rendered}"
        );
    }
    assert!(
        rendered.contains("… +6 lines") && !rendered.contains("second line 6"),
        "the sibling member's Fold is untouched: {rendered}"
    );
    assert!(
        rendered.contains("✓ Ran 2 commands"),
        "a member's Fold never collapses the Group: {rendered}"
    );

    application
        .handle_terminal_event(left_click_at(rendered_row(&rows, "✓ command 1") as u16))
        .expect("fold the member back from its header");
    let refolded = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert_eq!(
        refolded.matches("… +6 lines").count(),
        2,
        "the member folds back while the Group stays expanded: {refolded}"
    );
}

#[test]
fn a_members_fold_override_survives_collapse_and_re_expansion() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    set_member_output(&mut snapshot, "command 1", numbered_output(12));
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of successful commands");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 36);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
        ))
        .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 36);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&expanded_rows, "… +6 lines") as u16
        ))
        .expect("expand the member's Fold");
    let unfolded_rows = rendered_application_rows_at(&application, 80, 36);

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&unfolded_rows, "Ran 2 commands") as u16,
        ))
        .expect("collapse the Group over the unfolded member");
    let recollapsed_rows = rendered_application_rows_at(&application, 80, 36);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&recollapsed_rows, "Ran 2 commands") as u16,
        ))
        .expect("expand the Group again");

    let rendered = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert!(
        rendered.contains("output line 6") && !rendered.contains("… +"),
        "collapsing a Group never touches Fold state, so the member comes back \
         as the reader left it: {rendered}"
    );
}

#[test]
fn an_expanded_group_recollapses_only_from_its_header_row() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of successful commands");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 30);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
        ))
        .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 30);

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&expanded_rows, "output of command 2") as u16,
        ))
        .expect("click inside a member's output");
    let still_expanded = rendered_application_rows_at(&application, 80, 30).join("\n");
    assert!(
        still_expanded.contains("✓ command 2"),
        "a click below the header leaves the Group expanded: {still_expanded}"
    );

    application
        .handle_terminal_event(left_click_at(
            rendered_row(&expanded_rows, "Ran 3 commands") as u16,
        ))
        .expect("click the Group header");
    let collapsed = rendered_application_rows_at(&application, 80, 30).join("\n");
    assert!(
        collapsed.contains("✓ Ran 3 commands"),
        "the header remains after re-collapse: {collapsed}"
    );
    assert!(
        !collapsed.contains("command 1") && !collapsed.contains("output of command"),
        "re-collapsing hides the member rows again: {collapsed}"
    );
}

#[test]
fn group_state_stays_local_to_the_client_that_flipped_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND, SUCCESSFUL_COMMAND],
    );
    let mut reader = connected_application(workspace.path());
    let mut observer = connected_application(workspace.path());
    for application in [&mut reader, &mut observer] {
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .expect("attach both clients to the same Session");
    }
    let collapsed_rows = rendered_application_rows_at(&reader, 80, 30);

    reader
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
        ))
        .expect("one client expands the Group");

    assert!(
        rendered_application_rows_at(&reader, 80, 30)
            .join("\n")
            .contains("✓ command 1"),
        "the client that expanded sees the members"
    );
    let observed = rendered_application_rows_at(&observer, 80, 30).join("\n");
    assert!(
        observed.contains("✓ Ran 3 commands") && !observed.contains("command 1"),
        "Group state is client-local view state and never reaches another client: {observed}"
    );
}

#[test]
fn a_command_settling_successfully_is_absorbed_into_an_expanded_group() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let snapshot = live_command_run_snapshot(session_id, workspace.path());
    let running_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 30);
    application
        .handle_terminal_event(left_click_at(
            rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
        ))
        .expect("expand the Group while the Turn runs");
    let expanded_rows = rendered_application_rows_at(&application, 80, 30);
    let running = &expanded_rows[rendered_row(&expanded_rows, "$ command 3")];
    assert!(
        running.starts_with("    $ command 3"),
        "the running command stays outside the Group, in the standalone column: {running:?}"
    );

    application
        .handle_event(command_settles(
            session_id,
            next_revision,
            running_id,
            ActivityStatus::Completed,
            Some(0),
        ))
        .expect("project the command settling successfully");

    let rows = rendered_application_rows_at(&application, 80, 30);
    let rendered = rows.join("\n");
    assert!(
        rendered.contains("✓ Ran 3 commands"),
        "the expanded Group's header counts the absorbed member: {rendered}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("      ✓ command 3")),
        "the absorbed command renders as an indented member: {rendered}"
    );
    assert!(
        !rendered.contains("$ command 3"),
        "the standalone running row is gone: {rendered}"
    );
}
