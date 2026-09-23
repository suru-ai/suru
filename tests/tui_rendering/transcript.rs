//! Transcript projection, styling, and scroll navigation.

use crate::deadlines::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{
        buffer_rows, click_mouse, connected_application, connected_application_with_terminal_facts,
        failed_session_snapshot, navigable_session_snapshot, rendered_application_buffer,
        rendered_application_rows_at, rendered_row, text_position, workspace_dir,
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
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use suru::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, SessionEvent, SessionSubscription,
    },
    protocol::{
        Activity, ActivityId, ActivityStatus, AppearanceSettings, CommandAutoExpand, Cost,
        CostBasis, CreateSessionRequest, EffectiveSettings, FileChange, FoldPosture, InitialPrompt,
        Message, MessageId, MessageRole, MessageStatus, NativeMeter, Prompt, PromptDelivery,
        PromptId, PromptOrder, PromptStatus, ReasoningVisibility, SessionChange, SessionId,
        SessionRevision, SessionStatus, SessionTimestamp, SessionUpdate, SettingsSnapshot,
        TranscriptItem, TranscriptSettings, Turn, TurnId, TurnStatus, Usage,
    },
    server::{AgentOutput, ServerConfig},
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        TerminalFacts,
    },
};

/// Puts a fixture's Turn — and the Session running it — in flight, which is
/// what a reader watching that work is looking at. A settled Turn folds to its
/// marker, the subject of the Turn Fold tests, while these tests are about how
/// the entries inside a Turn disclose.
#[track_caller]
fn set_turn_in_flight(snapshot: &mut suru::protocol::SessionSnapshot, turn_id: TurnId) {
    snapshot.session.status = SessionStatus::Active;
    snapshot
        .turns
        .iter_mut()
        .find(|turn| turn.id == turn_id)
        .expect("fixture carries the Turn its Activities belong to")
        .status = TurnStatus::Active;
}

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

const EXTENDED_SGR_PAIRS: &str = concat!(
    "\x1b[38;5;201;48;5;22mindexed pair\x1b[0m ",
    "\x1b[38;2;1;2;3;48;2;4;5;6mtruecolor pair\x1b[0m ",
);

fn assert_extended_sgr_pairs(buffer: &Buffer) {
    let indexed = text_cell(buffer, "indexed pair");
    assert_eq!(indexed.fg, Color::Indexed(201));
    assert_eq!(indexed.bg, Color::Indexed(22));
    let truecolor = text_cell(buffer, "truecolor pair");
    assert_eq!(truecolor.fg, Color::Rgb(1, 2, 3));
    assert_eq!(truecolor.bg, Color::Rgb(4, 5, 6));
}

#[test]
fn wrapped_transcript_lines_keep_source_and_list_indentation() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::Agent)
        .expect("fixture contains an Agent Message")
        .content = concat!(
        "```text\n",
        "    indented-start alpha beta gamma delta epsilon zeta eta source-continuation\n",
        "```\n\n",
        "- bullet-start alpha beta gamma delta epsilon zeta eta bullet-continuation"
    )
    .to_owned();
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with wrapping Markdown");

    let buffer = rendered_application_buffer(&application, 50, 24);
    let rows = buffer_rows(&buffer);
    let continuation_column = |needle: &str| {
        rows.iter()
            .find(|row| row.contains(needle))
            .and_then(|row| row.chars().position(|character| !character.is_whitespace()))
            .unwrap_or_else(|| panic!("rendered frame contains {needle:?}")) as u16
    };
    assert_eq!(
        continuation_column("source-continuation"),
        text_position(&buffer, "indented-start").0,
        "a wrapped source line keeps its leading whitespace:\n{}",
        rows.join("\n")
    );
    assert_eq!(
        continuation_column("bullet-continuation"),
        text_position(&buffer, "bullet-start").0,
        "a wrapped list item uses a hanging indent beneath its text:\n{}",
        rows.join("\n")
    );
}

async fn apply_next_session_event(
    application: &mut Application,
    subscription: &mut SessionSubscription,
) -> SessionEvent {
    let event = tokio::time::timeout(PROGRESS_DEADLINE, subscription.next())
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
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
    press_leader_chord(&mut application, 'f');

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
    let workspace = workspace_dir();
    let application =
        application_with_ansi_palette_output(workspace.path(), TerminalFacts::default());
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
    assert_ansi_palette(&buffer, &normal, &bright);
}

#[test]
fn named_theme_base_ansi_foregrounds_and_backgrounds_use_its_derived_palette() {
    let workspace = workspace_dir();
    let mut application =
        application_with_ansi_palette_output(workspace.path(), TerminalFacts::unprobed(true));
    deliver_settings(
        &mut application,
        EffectiveSettings {
            appearance: AppearanceSettings {
                theme: "catppuccin".to_owned(),
                ..AppearanceSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &[],
    );

    let buffer = rendered_application_buffer(&application, 160, 30);
    let derived = [
        Color::Rgb(24, 24, 37),
        Color::Rgb(243, 139, 168),
        Color::Rgb(166, 227, 161),
        Color::Rgb(249, 226, 175),
        Color::Rgb(137, 180, 250),
        Color::Rgb(203, 166, 247),
        Color::Rgb(148, 226, 213),
        Color::Rgb(205, 214, 244),
    ];
    assert_ansi_palette(&buffer, &derived, &derived);
}

pub(super) fn application_with_ansi_palette_output(
    workspace: &std::path::Path,
    terminal_facts: TerminalFacts,
) -> Application {
    let mut application = Application::new(workspace, terminal_facts);
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace, 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
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
    press_leader_chord(&mut application, 'f');
    application
}

pub(super) fn assert_ansi_palette(buffer: &Buffer, normal: &[Color; 8], bright: &[Color; 8]) {
    for (index, expected) in normal.iter().copied().enumerate() {
        assert_eq!(text_cell(buffer, &format!("nfg{index}")).fg, expected);
        assert_eq!(text_cell(buffer, &format!("nbg{index}")).bg, expected);
    }
    for (index, expected) in bright.iter().copied().enumerate() {
        assert_eq!(text_cell(buffer, &format!("bfg{index}")).fg, expected);
        assert_eq!(text_cell(buffer, &format!("bbg{index}")).bg, expected);
    }
}

#[test]
fn escape_laden_transcript_stays_clean_after_scroll_and_session_switch() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let mut escaped = navigable_session_snapshot(SessionId::new(), workspace.path(), 8);
    let turn_id = escaped.turns[7].id;
    set_turn_in_flight(&mut escaped, turn_id);
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
    press_leader_chord(&mut application, 'f');

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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
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
            output: format!(
                "{EXTENDED_SGR_PAIRS}\x1b[38:2::7:8:9;48:5:42mcolon pair\x1b[m \
                 \x1b[2mdim text\x1b[0m \x1b[3mitalic text\x1b[0m"
            ),
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
    press_leader_chord(&mut application, 'f');

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

    assert_extended_sgr_pairs(&buffer);
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
fn named_themes_leave_extended_sgr_foregrounds_and_backgrounds_untouched() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), TerminalFacts::unprobed(true));
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    let activity = Activity::Command {
        id: ActivityId::new(),
        turn_id,
        status: ActivityStatus::Completed,
        command: "show extended colors".to_owned(),
        cwd: None,
        output: EXTENDED_SGR_PAIRS.to_owned(),
        output_truncated: false,
        exit_status: Some(0),
    };
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: activity.id(),
    });
    snapshot.activities.push(activity);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach Session with extended SGR colors");
    press_leader_chord(&mut application, 'f');
    deliver_settings(
        &mut application,
        EffectiveSettings {
            appearance: AppearanceSettings {
                theme: "catppuccin".to_owned(),
                ..AppearanceSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &[],
    );

    let buffer = rendered_application_buffer(&application, 120, 24);
    assert_extended_sgr_pairs(&buffer);
}

#[test]
fn command_output_osc_8_hyperlinks_render_with_link_style() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    let activity = Activity::Command {
        id: ActivityId::new(),
        turn_id,
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
    press_leader_chord(&mut application, 'f');

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
    let workspace = workspace_dir();
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
    let mut application = Application::new(workspace.path(), Default::default());
    for _ in 0..2 {
        application
            .handle_event(ApplicationEvent::Managed(
                client.next().await.expect("managed event arrives"),
            ))
            .expect("handle managed event");
    }

    let created = client
        .create_session(CreateSessionRequest {
 preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the stream\nwhile keeping this deliberately long user Message elevated across every wrapped continuation of the transcript block, including its semantic left accent"
                    .to_owned(),
                skill_invocations: Vec::new(),
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
                        skill_invocations: Vec::new(),
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id: Some(prompt_id),
                        agent: None,
                        status: TurnStatus::Active,
                        started_at: None,
                        settled_at: None,
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
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
                        skill_invocations: Vec::new(),
                    },
                },
            ],
        )
        .await
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
        .await
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
        .await
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
        ).await
        .expect("publish first Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;

    let partial = rendered_application_buffer(&application, 100, 35);
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
        .await
        .expect("publish final Agent Message chunk");
    apply_next_session_event(&mut application, &mut subscription).await;
    output
        .emit(session_id, AgentOutput::MessageCompleted { message_id })
        .await
        .expect("complete Agent Message");
    apply_next_session_event(&mut application, &mut subscription).await;

    let completed = rendered_application_buffer(&application, 100, 35);
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

    let user_row = rows
        .iter()
        .position(|row| row.contains('┃') && row.contains("Explain the stream"))
        .expect("the Prompt appears in the Transcript, as well as the header Title")
        as u16;
    let error_row = text_position(&completed, "Error:").1;
    let status_row = text_position(&completed, "Reading files").1;
    let agent_row = text_position(&completed, "Streamed heading").1;
    let code_start_row = text_position(&completed, "fn main() {").1;
    let code_after_blank_row = text_position(&completed, "println!(\"hi\");").1;
    let user_accent_column = rows[usize::from(user_row)]
        .chars()
        .position(|ch| ch == '┃')
        .expect("Prompt block accent") as u16;
    let normally_padded_width = completed.area.width.saturating_sub(4);
    let session_content_width = normally_padded_width.min(80);
    let user_block_right_edge = 2_u16
        .saturating_add(normally_padded_width.saturating_sub(session_content_width) / 2)
        .saturating_add(session_content_width.saturating_sub(1));
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
            Color::Reset,
            "the inherited surface spans the full user block width"
        );
    }
    assert_eq!(text_cell(&completed, "┃").fg, Color::Cyan);
    assert_eq!(
        completed
            .cell((user_accent_column + 2, user_row))
            .unwrap()
            .bg,
        Color::Reset
    );
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
    assert_eq!(text_cell(&completed, "link").fg, Color::Cyan);
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
    let workspace = workspace_dir();
    let cases = [
        (ActivityStatus::Active, None, "⠋ cargo test", Color::Cyan),
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
        let turn_id = snapshot.turns[0].id;
        set_turn_in_flight(&mut snapshot, turn_id);
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::Command {
            id: activity_id,
            turn_id,
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
        press_leader_chord(&mut application, 'f');
        if status == ActivityStatus::Active {
            let folded = rendered_application_rows_at(&application, 43, 18);
            left_click_at(&mut application, rendered_row(&folded, heading) as u16)
                .expect("open the active command whose details this matrix exercises");
        }

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
    let workspace = workspace_dir();
    let cases = [
        (
            ActivityStatus::Active,
            [
                "⠋ Renaming src/a.rs → src/b.rs",
                "⠋ Creating tests/new.rs",
                "⠋ Deleting old.rs",
                "⠋ Editing src/lib.rs",
            ],
            Color::Cyan,
        ),
        (
            ActivityStatus::Completed,
            [
                "✓ Renamed src/a.rs → src/b.rs",
                "✓ Created tests/new.rs",
                "✓ Deleted old.rs",
                "✓ Edited src/lib.rs",
            ],
            Color::Green,
        ),
        (
            ActivityStatus::Failed,
            [
                "× Failed to rename src/a.rs → src/b.rs",
                "× Failed to create tests/new.rs",
                "× Failed to delete old.rs",
                "× Failed to edit src/lib.rs",
            ],
            Color::Red,
        ),
    ];

    for (status, rows, color) in cases {
        let mut snapshot = failed_session_snapshot(
            SessionId::new(),
            PromptId::new(),
            "Change these files",
            workspace.path(),
        );
        let turn_id = snapshot.turns[0].id;
        set_turn_in_flight(&mut snapshot, turn_id);
        let activity_id = snapshot.activities[0].id();
        snapshot.activities[0] = Activity::FileChange {
            id: activity_id,
            turn_id,
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
                FileChange::Update {
                    path: "src/lib.rs".into(),
                    moved_to: None,
                },
            ],
        };
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with file-change Activity");

        let desktop = rendered_application_buffer(&application, 100, 22);
        let desktop_text = buffer_rows(&desktop).join("\n");
        for expected in rows {
            assert!(
                desktop_text.contains(expected),
                "desktop file-change Activity omitted {expected:?}:\n{desktop_text}"
            );
            let action = expected
                .split_once(' ')
                .and_then(|(_, rest)| rest.split_once(' '))
                .map_or(expected, |(action, _)| action);
            assert_eq!(text_cell(&desktop, action).fg, color);
        }
        assert_eq!(text_cell(&desktop, "src/a.rs").fg, Color::DarkGray);

        let compact = rendered_application_rows_at(&application, 43, 18).join("\n");
        for expected in rows {
            assert!(
                compact.contains(expected),
                "compact file-change Activity omitted {expected:?}:\n{compact}"
            );
        }
    }
}

#[test]
fn a_long_file_change_row_end_truncates_instead_of_wrapping() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Edit the deeply nested file",
        workspace.path(),
    );
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    snapshot.activities[0] = Activity::FileChange {
        id: snapshot.activities[0].id(),
        turn_id,
        status: ActivityStatus::Completed,
        changes: vec![FileChange::Update {
            path: "src/this/is/a/very/long/path/to/a/file/whose/name/does/not/fit.rs".into(),
            moved_to: None,
        }],
    };
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an overlong File Change row");

    let rows = rendered_application_rows_at(&application, 43, 18);
    let row = rows
        .iter()
        .find(|row| row.contains("✓ Edited"))
        .expect("the compact File Change row renders");

    assert!(
        row.trim_end().ends_with('…'),
        "the compact row ends in an ellipsis instead of wrapping: {row}"
    );
    assert!(
        !rows.join("\n").contains("does/not/fit.rs"),
        "the clipped path tail never wraps onto another row: {rows:?}"
    );
}

#[test]
fn streaming_command_updates_reuse_one_folded_projected_transcript_row() {
    let workspace = workspace_dir();
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
    assert_eq!(streamed.matches("⠋ cargo test").count(), 1);
    assert!(
        !streamed.contains("running tests"),
        "streaming output stays behind the command's one projected row: {streamed}"
    );

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
    assert!(!completed.contains("⠋ cargo test"));
    assert!(
        !completed.contains("running tests"),
        "the settled command folds its output away: {completed}"
    );
}

#[test]
fn page_up_exposes_latest_and_control_end_resumes_following_the_transcript() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
        .expect("page away again before using Ctrl+End");
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
        .expect("End moves in the composer");
    assert!(
        rendered_application_rows_at(&application, 72, 18)
            .join("\n")
            .contains("Latest ↓ · Ctrl+End"),
        "End leaves the Transcript scrolled and the hint names Ctrl+End"
    );
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::CONTROL,
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
    let workspace = workspace_dir();
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

    let mut reader = Application::new(workspace.path(), Default::default());
    reader
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach historical reader");
    let mut observer = Application::new(workspace.path(), Default::default());
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
    let anchor_row = rendered_row(&anchored_rows, "Agent section 4");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 4"),
        "expected the fourth Agent Message to be the visible anchor:\n{anchored}"
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
        rendered_row(&resized_reader_rows, "Agent section 4"),
        anchor_row,
        "resize must keep the Message on the same rendered row"
    );
    let resized_reader = resized_reader_rows.join("\n");
    assert!(
        resized_reader.contains("Agent section 4"),
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
                settled_at: None,
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
        rendered_row(&completed_reader_rows, "Agent section 4"),
        anchor_row,
        "completion must keep the Message on the same rendered row"
    );
    let completed_reader = completed_reader_rows.join("\n");
    assert!(completed_reader.contains("Agent section 4"));
    assert!(completed_reader.contains("Latest"));
    assert!(
        rendered_application_rows_at(&observer, 56, 20)
            .join("\n")
            .contains("STREAMED TAIL")
    );
}

#[test]
fn resize_that_reveals_the_whole_transcript_resumes_following() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
            KeyModifiers::CONTROL,
        )))
        .expect("return to the very long tail");
    let resumed = rendered_application_rows_at(&application, 72, 18).join("\n");
    assert!(resumed.contains("TAIL beyond u16"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn transcript_navigation_reaches_tail_of_one_oversized_wrapped_line() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
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
            KeyModifiers::CONTROL,
        )))
        .expect("return to the oversized wrapped line tail");
    let resumed = rendered_application_rows_at(&application, terminal_width, 18).join("\n");
    assert!(resumed.contains("TAIL"));
    assert!(!resumed.contains("Latest"));
}

#[test]
fn message_anchor_survives_prompt_reconciliation_and_composer_dock_layout_changes() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach a long Session");
    rendered_application_rows_at(&application, 72, 23);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page into transcript history");
    let anchored_rows = rendered_application_rows_at(&application, 72, 23);
    let anchor_row = rendered_row(&anchored_rows, "Agent section 2");
    let anchored = anchored_rows.join("\n");
    assert!(
        anchored.contains("Agent section 2"),
        "expected the second Agent Message to be the visible anchor:\n{anchored}"
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Reconciled line one\nline two\nline three\nline four\nline five\nfinal draft row"
                .to_owned(),
        )))
        .expect("grow the multiline composer");
    let growing_composer_rows = rendered_application_rows_at(&application, 72, 23);
    assert_eq!(
        rendered_row(&growing_composer_rows, "Agent section 2"),
        anchor_row,
        "composer growth must keep the Message on the same rendered row"
    );
    let growing_composer = growing_composer_rows.join("\n");
    assert!(growing_composer.contains("Agent section 2"));
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
                        skill_invocations: Vec::new(),
                    },
                }],
            },
        )))
        .expect("show a queued Prompt dock");
    let with_dock_rows = rendered_application_rows_at(&application, 72, 23);
    assert_eq!(
        rendered_row(&with_dock_rows, "Agent section 2"),
        anchor_row,
        "queued dock changes must keep the Message on the same rendered row"
    );
    let with_dock = with_dock_rows.join("\n");
    assert!(with_dock.contains("Agent section 2"));
    assert!(with_dock.contains("Queued dock entry"));
    assert!(with_dock.contains("Latest"));

    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the multiline steer optimistically")
    else {
        panic!("a Session steer should request Prompt admission");
    };
    let provisional_rows = rendered_application_rows_at(&application, 72, 23);
    assert_eq!(
        rendered_row(&provisional_rows, "Agent section 2"),
        anchor_row,
        "provisional content must keep the Message on the same rendered row"
    );
    let provisional = provisional_rows.join("\n");
    assert!(provisional.contains("Agent section 2"));
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
                            skill_invocations: Vec::new(),
                        },
                    },
                    SessionChange::TurnAdded {
                        turn: Turn {
                            id: delivered_turn_id,
                            prompt_id: Some(request.prompt.id),
                            agent: None,
                            status: TurnStatus::Active,
                            started_at: None,
                            settled_at: None,
                            last_output_at: None,
                            usage: None,
                            cost: None,
                            cost_basis: None,
                            cost_details: None,
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
                            skill_invocations: Vec::new(),
                        },
                    },
                ],
            },
        )))
        .expect("reconcile the optimistic Prompt to a stable Message");
    let reconciled_anchor_rows = rendered_application_rows_at(&application, 72, 23);
    assert_eq!(
        rendered_row(&reconciled_anchor_rows, "Agent section 2"),
        anchor_row,
        "Message reconciliation must keep the anchor on the same rendered row"
    );
    let reconciled_anchor = reconciled_anchor_rows.join("\n");
    assert!(reconciled_anchor.contains("Agent section 2"));
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
    let latest = rendered_application_rows_at(&application, 72, 23).join("\n");
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
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Command {
        id: activity_id,
        turn_id,
        status,
        command: "cargo test".to_owned(),
        cwd: None,
        output: output.to_owned(),
        output_truncated,
        exit_status: match status {
            ActivityStatus::Active | ActivityStatus::Interrupted => None,
            ActivityStatus::Completed | ActivityStatus::Failed => Some(0),
        },
    };
    (snapshot, activity_id)
}

#[test]
fn an_expanded_command_wraps_beneath_its_text_and_nests_its_details() {
    let workspace = workspace_dir();
    let (mut snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        "output-start",
        false,
    );
    let Activity::Command { command, cwd, .. } = &mut snapshot.activities[0] else {
        panic!("the Session's Activity is a command");
    };
    *command =
        "command-start alpha beta gamma delta epsilon zeta eta command-continuation".to_owned();
    *cwd = Some("/workspace-start".into());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an overlong command");
    press_leader_chord(&mut application, 'f');

    let buffer = rendered_application_buffer(&application, 50, 24);
    let rows = buffer_rows(&buffer);
    let first_occupied_column = |needle: &str| {
        rows.iter()
            .find(|row| row.contains(needle))
            .and_then(|row| row.chars().position(|character| !character.is_whitespace()))
            .unwrap_or_else(|| panic!("rendered frame contains {needle:?}")) as u16
    };
    let command_column = text_position(&buffer, "command-start").0;
    assert_eq!(
        first_occupied_column("command-continuation"),
        command_column,
        "the command continuation hangs beneath the command text:\n{}",
        rows.join("\n")
    );
    for detail in ["in /workspace-start", "output-start"] {
        assert_eq!(
            text_position(&buffer, detail).0,
            command_column + 2,
            "command details sit one level beneath the command text:\n{}",
            rows.join("\n")
        );
    }
}

fn numbered_output(lines: usize) -> String {
    prefixed_output("output", lines)
}

fn left_click_at(application: &mut Application, row: u16) -> anyhow::Result<ApplicationTransition> {
    super::support::click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 6,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
}

/// Presses the Ctrl+X leader chord followed by `key`, the way a reader
/// invokes a leader-bound semantic command.
fn press_leader_chord(application: &mut Application, key: char) {
    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("invoke the leader-bound semantic command");
    }
}

#[test]
fn a_settled_command_folds_to_a_single_row() {
    let workspace = workspace_dir();
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

    assert!(
        folded.contains("✓ cargo test"),
        "the folded row keeps the status marker and command: {folded}"
    );
    assert!(
        !folded.contains("output line"),
        "a folded command shows none of its output: {folded}"
    );
    assert!(
        !folded.contains("… +"),
        "the single folded row carries no fold marker: {folded}"
    );
}

#[test]
fn a_folded_command_row_end_truncates_instead_of_wrapping() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(3),
        false,
    );
    assert_eq!(snapshot.activities[0].id(), activity_id);
    let Activity::Command { command, .. } = &mut snapshot.activities[0] else {
        panic!("the session's Activity is a command");
    };
    *command =
        "cargo run --release --bin very-long-binary-name --features one,two,three".to_owned();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an overlong command");

    let rows = rendered_application_rows_at(&application, 40, 24);
    let header = rows
        .iter()
        .find(|row| row.contains("✓ cargo run"))
        .expect("the folded command row renders");

    assert!(
        header.trim_end().ends_with('…'),
        "the folded row ends in an ellipsis instead of wrapping: {header}"
    );
    assert!(
        !rows.join("\n").contains("--features"),
        "the clipped end of the command never renders: {rows:?}"
    );
}

#[test]
fn the_fold_marker_counts_logical_lines_so_it_reads_the_same_at_every_width() {
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the command's Peek");

    let narrow = rendered_application_rows_at(&application, 44, 24).join("\n");
    let wide = rendered_application_rows_at(&application, 110, 24).join("\n");

    assert!(narrow.contains("… +6 lines"), "narrow frame: {narrow}");
    assert!(wide.contains("… +6 lines"), "wide frame: {wide}");
}

#[test]
fn long_output_lines_wrap_before_the_clamp_so_a_few_cannot_flood_the_fold() {
    let workspace = workspace_dir();
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
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the command's Peek");

    let peek = rendered_application_rows_at(&application, 60, 24).join("\n");

    assert!(
        peek.contains("… +3 lines"),
        "the marker counts the three source lines the wrapped-row budget hides: {peek}"
    );
    assert!(
        peek.contains("line 4") && !peek.contains("line 3 "),
        "the Peek keeps only the tail the row budget allows: {peek}"
    );
}

#[test]
fn a_fold_click_waits_for_release_and_a_drag_never_toggles_it() {
    use crossterm::event::MouseButton;

    for (motion_column, release_column, is_click) in [
        (None, 6, true),
        (Some(6), 6, true),
        (None, 7, false),
        (Some(7), 7, false),
        (Some(7), 6, false),
    ] {
        let workspace = workspace_dir();
        let (snapshot, _) = command_activity_session(
            workspace.path(),
            ActivityStatus::Completed,
            &numbered_output(12),
            false,
        );
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .unwrap();
        let before = rendered_application_rows_at(&application, 60, 24);
        let row = rendered_row(&before, "✓ cargo test") as u16;
        let mouse = |kind, column| {
            InputEvent::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        application
            .handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), 6))
            .unwrap();
        assert_eq!(
            rendered_application_rows_at(&application, 60, 24),
            before,
            "pressing alone must not open the Fold"
        );
        if let Some(column) = motion_column {
            application
                .handle_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), column))
                .unwrap();
            assert_eq!(rendered_application_rows_at(&application, 60, 24), before);
        }
        application
            .handle_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), release_column))
            .unwrap();
        let after = rendered_application_rows_at(&application, 60, 24);
        if !is_click {
            assert_eq!(
                after, before,
                "a drag must not toggle, even when it returns to the anchor"
            );
        } else {
            assert!(
                after.join("\n").contains("output line 12"),
                "a click opens the Fold"
            );
        }
    }
}

#[test]
fn a_command_fold_opens_in_stages_and_folds_back_from_the_header() {
    let workspace = workspace_dir();
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

    assert_eq!(
        left_click_at(
            &mut application,
            rendered_row(&folded_rows, "✓ cargo test") as u16
        )
        .expect("click the folded row"),
        ApplicationTransition::Continue
    );
    let peek_rows = rendered_application_rows_at(&application, 60, 24);
    let peek = peek_rows.join("\n");
    for tail in ["output line 7", "output line 12"] {
        assert!(peek.contains(tail), "the Peek shows the tail: {peek}");
    }
    for hidden in ["output line 1 ", "output line 6"] {
        assert!(!peek.contains(hidden), "the Peek hides the head: {peek}");
    }
    assert!(
        peek.contains("… +6 lines"),
        "the Peek counts what it still hides: {peek}"
    );
    assert!(
        rendered_row(&peek_rows, "… +6 lines") < rendered_row(&peek_rows, "output line 7"),
        "the fold marker sits above the tail it stands in for: {peek}"
    );

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "… +6 lines") as u16,
    )
    .expect("click the fold marker");
    let expanded_rows = rendered_application_rows_at(&application, 60, 24);
    let expanded = expanded_rows.join("\n");
    for line in 1..=12 {
        assert!(
            expanded.contains(&format!("output line {line}")),
            "the marker opens the Fold the rest of the way: {expanded}"
        );
    }
    assert!(
        !expanded.contains("… +"),
        "no fold marker remains: {expanded}"
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "✓ cargo test") as u16,
    )
    .expect("click the entry header");
    let refolded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        refolded.contains("✓ cargo test") && !refolded.contains("output line"),
        "clicking the header folds the entry back to its single row: {refolded}"
    );
}

#[test]
fn a_failed_command_opens_to_its_peek_by_default() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Failed,
        &numbered_output(12),
        false,
    );
    assert_eq!(snapshot.activities[0].id(), activity_id);
    let Activity::Command { exit_status, .. } = &mut snapshot.activities[0] else {
        panic!("the session's Activity is a command");
    };
    *exit_status = Some(3);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a failed command");

    let rendered = rendered_application_rows_at(&application, 60, 24).join("\n");

    assert!(
        rendered.contains("× cargo test (exit 3)"),
        "the failed header names its exit status: {rendered}"
    );
    assert!(
        rendered.contains("… +6 lines") && rendered.contains("output line 12"),
        "a failed command opens to its Peek, where the error lives: {rendered}"
    );
    assert!(
        !rendered.contains("output line 6"),
        "the Peek still keeps the head behind its marker: {rendered}"
    );
}

#[test]
fn expanding_failed_command_blank_output_keeps_working_tail_visible() {
    let workspace = workspace_dir();
    let output = format!("{}\n\n{}", numbered_output(9), prefixed_output("tail", 3));
    let (mut snapshot, _) =
        command_activity_session(workspace.path(), ActivityStatus::Failed, &output, false);
    snapshot.session.working_since = Some(SessionTimestamp::now());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Working Session with blank command output");

    let peek_rows = rendered_application_rows_at(&application, 60, 24);
    let peek = peek_rows.join("\n");
    assert!(
        peek.contains("Working ("),
        "the Peek keeps the tail: {peek}"
    );
    assert!(
        !peek.contains("Latest ↓"),
        "a view following the tail has no Latest affordance: {peek}"
    );

    left_click_at(&mut application, rendered_row(&peek_rows, "… +") as u16)
        .expect("expand the failed command from its Peek");

    let expanded_rows = rendered_application_rows_at(&application, 60, 24);
    let expanded = expanded_rows.join("\n");
    assert!(
        expanded.contains("Working ("),
        "blank expanded output cannot clip the Working tail: {expanded}"
    );
    assert!(
        !expanded.contains("Latest ↓"),
        "expanding a Fold does not leave the followed-latest position: {expanded}"
    );

    let resized_rows = rendered_application_rows_at(&application, 72, 20);
    let resized = resized_rows.join("\n");
    assert!(
        resized.contains("Working (") && !resized.contains("Latest ↓"),
        "resizing a followed-latest view keeps its Working tail: {resized}"
    );

    let restored_rows = rendered_application_rows_at(&application, 60, 24);
    left_click_at(
        &mut application,
        rendered_row(&restored_rows, "× cargo test") as u16,
    )
    .expect("collapse the expanded failed command");
    let collapsed = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        collapsed.contains("Working (") && !collapsed.contains("Latest ↓"),
        "collapsing the Fold keeps the followed Working tail: {collapsed}"
    );
}

#[test]
fn streaming_blank_command_output_keeps_working_tail_visible() {
    let workspace = workspace_dir();
    let output = format!("{}\n\n{}", numbered_output(9), prefixed_output("tail", 3));
    let (mut snapshot, activity_id) =
        command_activity_session(workspace.path(), ActivityStatus::Active, &output, false);
    snapshot.session.working_since = Some(SessionTimestamp::now());
    let session_id = snapshot.session.id;
    let revision = snapshot.revision;
    let mut application = session_opened_under(
        workspace.path(),
        EffectiveSettings {
            transcript: TranscriptSettings {
                command_auto_expand: CommandAutoExpand::AfterMillis(0),
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["transcript.commandAutoExpand"],
        snapshot,
    );
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("promote the streaming command to its live tail");

    let before = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        before.contains("Working (") && !before.contains("Latest ↓"),
        "expanded blank output keeps the followed Working tail: {before}"
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: "\n\nstreamed tail".to_owned(),
                }],
            },
        )))
        .expect("grow the streaming command through another blank line");

    let grown = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        grown.contains("streamed tail"),
        "streamed output arrives: {grown}"
    );
    assert!(
        grown.contains("Working (") && !grown.contains("Latest ↓"),
        "blank streaming growth keeps the followed Working tail: {grown}"
    );
}

#[test]
fn blank_status_and_error_lines_keep_working_tail_visible() {
    let workspace = workspace_dir();
    let text = format!(
        "{}\n\n{}",
        prefixed_output("detail", 9),
        prefixed_output("tail", 3)
    );
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Report progress",
        workspace.path(),
    );
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    snapshot.session.working_since = Some(SessionTimestamp::now());
    snapshot.activities[0] = Activity::Error {
        id: snapshot.activities[0].id(),
        turn_id,
        text: text.clone(),
    };
    let status_id = ActivityId::new();
    snapshot.activities.push(Activity::Status {
        id: status_id,
        turn_id,
        text,
    });
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: status_id,
    });
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Working Session with blank Status and Error lines");

    let rendered = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        rendered.contains("Working (") && !rendered.contains("Latest ↓"),
        "blank Status and Error lines keep the followed Working tail: {rendered}"
    );
}

#[test]
fn a_command_interrupted_without_being_watched_folds_to_its_single_row() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Failed,
        &numbered_output(12),
        false,
    );
    assert_eq!(snapshot.activities[0].id(), activity_id);
    let Activity::Command { exit_status, .. } = &mut snapshot.activities[0] else {
        panic!("the session's Activity is a command");
    };
    *exit_status = None;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an interrupted command");

    let rendered = rendered_application_rows_at(&application, 60, 24).join("\n");

    assert!(
        rendered.contains("× cargo test") && !rendered.contains("(exit"),
        "the interrupted row reports no exit status: {rendered}"
    );
    assert!(
        !rendered.contains("output line") && !rendered.contains("… +"),
        "without the watcher's interrupt override, an interrupted command folds \
         like a success — its Peek is the override's doing, not a default: {rendered}"
    );
}

#[test]
fn a_folded_failed_row_keeps_its_exit_suffix_past_the_clamp() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Failed,
        &numbered_output(12),
        false,
    );
    assert_eq!(snapshot.activities[0].id(), activity_id);
    let Activity::Command {
        command,
        exit_status,
        ..
    } = &mut snapshot.activities[0]
    else {
        panic!("the session's Activity is a command");
    };
    *command =
        "cargo run --release --bin very-long-binary-name --features one,two,three".to_owned();
    *exit_status = Some(17);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a long failed command");
    let peek_rows = rendered_application_rows_at(&application, 40, 24);

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "× cargo run") as u16,
    )
    .expect("fold the failed command to its single row");

    let rows = rendered_application_rows_at(&application, 40, 24);
    let header = rows
        .iter()
        .find(|row| row.contains("× cargo run"))
        .expect("the folded failed row renders");
    assert!(
        header.trim_end().ends_with("… (exit 17)"),
        "the clamp eats the command's tail, never the exit suffix: {header}"
    );
}

#[test]
fn the_folded_row_hides_the_cwd_line_until_the_peek() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    assert_eq!(snapshot.activities[0].id(), activity_id);
    let Activity::Command { cwd, .. } = &mut snapshot.activities[0] else {
        panic!("the session's Activity is a command");
    };
    *cwd = Some("/fixture/work".into());
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a command that has a cwd");

    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    assert!(
        !folded_rows.join("\n").contains("in /fixture/work"),
        "the single folded row keeps the cwd line back: {folded_rows:?}"
    );

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the command's Peek");
    let peek = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        peek.contains("in /fixture/work"),
        "the Peek brings the full header back, cwd included: {peek}"
    );
}

#[test]
fn a_peek_that_fits_everything_shows_no_marker_and_folds_back_from_its_header() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(4),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a short-output command");
    let folded_rows = rendered_application_rows_at(&application, 60, 24);

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the command's Peek");
    let peek_rows = rendered_application_rows_at(&application, 60, 24);
    let peek = peek_rows.join("\n");
    for line in 1..=4 {
        assert!(
            peek.contains(&format!("output line {line}")),
            "a Peek whose budget fits everything shows it all: {peek}"
        );
    }
    assert!(
        !peek.contains("… +"),
        "a fold marker never says +0 lines: {peek}"
    );

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "output line 2") as u16,
    )
    .expect("click the revealed output");
    assert!(
        rendered_application_rows_at(&application, 60, 24)
            .join("\n")
            .contains("output line 2"),
        "with nothing left to reveal, an output click changes nothing"
    );

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "✓ cargo test") as u16,
    )
    .expect("click the entry header");
    let refolded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        !refolded.contains("output line"),
        "the header folds the merged Peek straight back to its single row: {refolded}"
    );
}

#[test]
fn clicks_on_revealed_output_change_nothing() {
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the command's Peek");
    let peek_rows = rendered_application_rows_at(&application, 60, 24);

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "output line 9") as u16,
    )
    .expect("click a revealed tail line");
    let after_peek_click = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        after_peek_click.contains("… +6 lines") && !after_peek_click.contains("output line 6"),
        "a click on the Peek's output moves nothing, keeping the surface free for selection: {after_peek_click}"
    );

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "… +6 lines") as u16,
    )
    .expect("open the Fold the rest of the way");
    let expanded_rows = rendered_application_rows_at(&application, 60, 24);
    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "output line 6") as u16,
    )
    .expect("click inside the fully revealed output");

    let after = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        after.contains("output line 6") && !after.contains("… +"),
        "a click on output never folds away the content under the pointer: {after}"
    );
}

#[test]
fn toggling_the_fold_posture_expands_every_entry_and_clears_per_entry_overrides() {
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open one entry's Peek by hand");

    press_leader_chord(&mut application, 'f');
    let expanded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        expanded.contains("output line 6") && !expanded.contains("… +"),
        "the expanded posture shows every entry in full: {expanded}"
    );

    press_leader_chord(&mut application, 'f');
    let refolded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        refolded.contains("✓ cargo test") && !refolded.contains("output line"),
        "flipping back folds the entry the reader had opened by hand: {refolded}"
    );
}

/// A client that received the effective-settings snapshot on connect and then
/// opened `snapshot`, which is the order the protocol guarantees: the snapshot
/// is the first event on the lifecycle stream, and attaching a Session takes a
/// reader's action after that.
fn session_opened_under(
    workspace: &std::path::Path,
    mut settings: EffectiveSettings,
    pinned: &[&str],
    snapshot: suru::protocol::SessionSnapshot,
) -> Application {
    // These fixtures exercise Transcript behavior at their stated frame
    // widths. Keep the adjacent Sidebar out of that width unless a test is
    // explicitly about composing the two surfaces.
    settings.sidebar.initial_visibility = suru::protocol::SidebarVisibility::Hidden;
    let mut application = connected_application(workspace);
    deliver_settings(&mut application, settings, pinned);
    crate::support::hide_aside(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open a Session view under the pinned Settings");
    application
}

/// Pushes an effective-settings snapshot at a client, the way the server does
/// on connect and after every accepted edit.
fn deliver_settings(application: &mut Application, settings: EffectiveSettings, pinned: &[&str]) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings,
                pinned: pinned.iter().map(|key| (*key).to_owned()).collect(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the effective-settings snapshot");
}

/// The same client, opened under one pinned default Fold posture — and with
/// Reasoning shown, because a posture is only visible in entries the reader is
/// shown and these fixtures think as well as work.
fn session_opened_at(
    workspace: &std::path::Path,
    posture: FoldPosture,
    snapshot: suru::protocol::SessionSnapshot,
) -> Application {
    session_opened_under(
        workspace,
        EffectiveSettings {
            transcript: TranscriptSettings {
                default_fold_posture: posture,
                reasoning_visibility: ReasoningVisibility::Shown,
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &[
            "transcript.defaultFoldPosture",
            "transcript.reasoningVisibility",
        ],
        snapshot,
    )
}

/// A connected client whose reader asked to see Reasoning. Suru hides it by
/// default — a Transcript leads with the work and the answer rather than the
/// account of how the agent got there — so every test about how a Reasoning
/// row, Group, or Fold presents starts by turning it on.
fn client_showing_reasoning(workspace: &std::path::Path) -> Application {
    let mut application = connected_application(workspace);
    let mut settings = settings_with_reasoning(ReasoningVisibility::Shown);
    // Initial visibility is seeded by the first Settings snapshot, so hide
    // the Sidebar here rather than trying to correct the fixture later.
    settings.sidebar.initial_visibility = suru::protocol::SidebarVisibility::Hidden;
    deliver_settings(
        &mut application,
        settings,
        &["transcript.reasoningVisibility"],
    );
    crate::support::hide_aside(&mut application);
    application
}

/// Effective settings whose only departure from the built-in defaults is
/// whether a Transcript shows Reasoning at all.
fn settings_with_reasoning(visibility: ReasoningVisibility) -> EffectiveSettings {
    EffectiveSettings {
        transcript: TranscriptSettings {
            reasoning_visibility: visibility,
            ..TranscriptSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

#[test]
fn the_pinned_default_fold_posture_opens_a_fresh_session_view_expanded() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );

    let application = session_opened_at(workspace.path(), FoldPosture::Expanded, snapshot);

    let opened = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        opened.contains("output line 6") && !opened.contains("… +"),
        "the pinned expanded posture opens the view with every Fold expanded: {opened}"
    );
}

#[test]
fn the_folded_default_fold_posture_keeps_a_fresh_session_view_folded() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );

    let application = session_opened_at(workspace.path(), FoldPosture::Folded, snapshot);

    let opened = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        opened.contains("✓ cargo test") && !opened.contains("output line"),
        "the folded posture opens the view the way an unpinned Setting does: {opened}"
    );
}

#[test]
fn the_expanded_default_fold_posture_leaves_the_fold_toggle_flipping_as_before() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = session_opened_at(workspace.path(), FoldPosture::Expanded, snapshot);

    press_leader_chord(&mut application, 'f');
    let folded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        folded.contains("✓ cargo test") && !folded.contains("output line"),
        "the toggle folds the view away from the pinned posture: {folded}"
    );

    press_leader_chord(&mut application, 'f');
    let expanded = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        expanded.contains("output line 6") && !expanded.contains("… +"),
        "flipping back reaches the expanded posture again: {expanded}"
    );
}

#[test]
fn the_expanded_default_fold_posture_leaves_the_turn_fold_axis_alone() {
    let workspace = workspace_dir();
    let snapshot = settled_turn_session(
        workspace.path(),
        "Run the workflow",
        "The workflow is green.",
    );
    let mut application = session_opened_at(workspace.path(), FoldPosture::Expanded, snapshot);

    let opened = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert!(
        opened.contains("✓ Worked") && !opened.contains("Reading the workflow"),
        "the Fold Setting speaks for Folds alone: a settled Turn still stands \
         as its Turn Fold marker: {opened}"
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::TranscriptTurnsToggle,
        )))
        .expect("open every Turn Fold");

    let expanded = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert!(
        expanded.contains("Reading the workflow"),
        "the Turn posture toggle opens every Turn Fold as it did before: {expanded}"
    );
}

#[test]
fn error_and_status_activities_are_never_folded() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Run the test suite",
        workspace.path(),
    );
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
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
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Change these files",
        workspace.path(),
    );
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    snapshot.activities[0] = Activity::FileChange {
        id: snapshot.activities[0].id(),
        turn_id,
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
        folded.contains("✓ Created src/file4.rs"),
        "the budget lists four paths: {folded}"
    );
    assert!(
        !folded.contains("✓ Created src/file5.rs"),
        "paths past the budget are folded away: {folded}"
    );
    assert!(
        folded.contains("… +3 more"),
        "the fold marker counts the paths it hides: {folded}"
    );

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "… +3 more") as u16,
    )
    .expect("expand the file-change entry");
    let expanded = rendered_application_rows_at(&application, 60, 24).join("\n");
    for change in 1..=7 {
        assert!(
            expanded.contains(&format!("✓ Created src/file{change}.rs")),
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
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Reasoning {
        id: activity_id,
        turn_id,
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
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        Some("Inspecting the seam"),
        "Reading the projection.\n\nThen the store.",
        Some(72_000),
    );
    let mut application = client_showing_reasoning(workspace.path());
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

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "Thought: Inspecting the seam") as u16,
    )
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
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        Some("Inspecting the seam"),
        "Reading the projection.",
        None,
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a streaming Reasoning Activity");

    let streaming_rows = rendered_application_rows_at(&application, 72, 24);
    let streaming = streaming_rows.join("\n");

    let header = &streaming_rows[rendered_row(&streaming_rows, "Thinking: Inspecting the seam")];
    assert_eq!(
        header.trim_end(),
        "    ⠋ Thinking: Inspecting the seam · +1 lines",
        "a Reasoning block still running heads with its running label and no duration"
    );
    assert!(
        !streaming.contains("Reading the projection."),
        "a Reasoning block still running is folded like any other: {streaming}"
    );
}

#[test]
fn untitled_reasoning_heads_with_the_label_alone() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        None,
        "Reading the projection.",
        Some(4_200),
    );
    let mut application = client_showing_reasoning(workspace.path());
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
fn a_reasoning_block_that_settled_empty_renders_no_row_in_either_posture() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        None,
        "",
        Some(276),
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a Reasoning block the Provider never described");

    let folded = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert!(
        folded.contains("Explain the Transcript"),
        "the rest of the Transcript still renders: {folded}"
    );
    assert_reasoning_shows_nothing(
        &folded,
        "a Reasoning block that settled with no title and no content shows nothing",
    );

    press_leader_chord(&mut application, 'f');
    let expanded = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert_reasoning_shows_nothing(
        &expanded,
        "the expanded posture has nothing to open for an empty block",
    );
}

/// Asserts a rendered Transcript carries no trace of a Reasoning block: no
/// header in either of its wordings, and none of the fixture duration that
/// would ride one.
#[track_caller]
fn assert_reasoning_shows_nothing(rendered: &str, why: &str) {
    for absent in ["Thought", "Thinking", "276ms"] {
        assert!(
            !rendered.contains(absent),
            "{why}, but {absent:?} rendered: {rendered}"
        );
    }
}

#[test]
fn hidden_reasoning_shows_no_row_however_fully_the_provider_described_it() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        Some("Inspecting the seam"),
        "Reading the projection.",
        Some(72_000),
    );
    let mut application = session_opened_under(
        workspace.path(),
        settings_with_reasoning(ReasoningVisibility::Hidden),
        &["transcript.reasoningVisibility"],
        snapshot,
    );

    let folded = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert!(
        folded.contains("Explain the Transcript"),
        "the rest of the Transcript still renders: {folded}"
    );
    for absent in ["Thought", "Thinking", "Inspecting the seam", "1m 12s"] {
        assert!(
            !folded.contains(absent),
            "a hidden Reasoning block draws no row at all, but {absent:?} rendered: {folded}"
        );
    }

    press_leader_chord(&mut application, 'f');
    let expanded = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert!(
        !expanded.contains("Reading the projection."),
        "the expanded posture has nothing to open for a hidden block: {expanded}"
    );
}

#[test]
fn reasoning_stays_hidden_until_a_reader_asks_for_it_and_arrives_in_the_open_view() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        Some("Inspecting the seam"),
        "Reading the projection.",
        Some(72_000),
    );
    let mut application = session_opened_under(
        workspace.path(),
        EffectiveSettings::default(),
        &[],
        snapshot,
    );
    assert!(
        !rendered_application_rows_at(&application, 72, 24)
            .join("\n")
            .contains("Thought"),
        "the built-in default keeps thinking out of the Transcript"
    );

    deliver_settings(
        &mut application,
        settings_with_reasoning(ReasoningVisibility::Shown),
        &["transcript.reasoningVisibility"],
    );
    assert!(
        rendered_application_rows_at(&application, 72, 24)
            .join("\n")
            .contains("Thought: Inspecting the seam"),
        "asking for Reasoning reaches the view the reader already has open — \
         unlike a default Fold posture, which only decides where a view starts — \
         and brings back the block that arrived while it was hidden"
    );

    deliver_settings(&mut application, EffectiveSettings::default(), &[]);
    assert!(
        !rendered_application_rows_at(&application, 72, 24)
            .join("\n")
            .contains("Thought"),
        "unpinning the Setting returns the Transcript to its quiet default"
    );
}

#[test]
fn a_reasoning_block_interrupted_before_it_said_anything_renders_no_row() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        reasoning_activity_session(workspace.path(), ActivityStatus::Failed, None, "", None);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning block was cut off before it said anything");

    let rendered = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert_reasoning_shows_nothing(
        &rendered,
        "a block cut off before the Provider described it settled with nothing to show",
    );
    assert!(
        !rendered.contains("interrupted"),
        "how the thinking ended is the Turn's story, not an empty block's: {rendered}"
    );
}

#[test]
fn an_interrupted_reasoning_block_that_said_something_keeps_its_row() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Failed,
        None,
        "Reading the projection.",
        None,
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning block was cut off mid-summary");

    let rows = rendered_application_rows_at(&application, 72, 24);
    assert_eq!(
        rows[rendered_row(&rows, "Thinking interrupted")].trim_end(),
        "    × Thinking interrupted · +1 lines",
        "a block that said something before it was cut off still stands for what it said"
    );
}

#[test]
fn a_reasoning_block_whose_content_the_cap_dropped_keeps_its_row() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        None,
        "",
        Some(276),
    );
    let Some(Activity::Reasoning {
        content_truncated, ..
    }) = snapshot
        .activities
        .iter_mut()
        .find(|activity| activity.id() == activity_id)
    else {
        panic!("the fixture's Activity is the Reasoning block");
    };
    *content_truncated = true;
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning block lost its content to the cap");

    let rendered = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert!(
        rendered.contains("Thought · 276ms"),
        "content the cap cut away still happened, so the block keeps its row: {rendered}"
    );

    press_leader_chord(&mut application, 'f');
    let expanded = rendered_application_rows_at(&application, 72, 24).join("\n");
    assert!(
        expanded.contains("[Reasoning truncated]"),
        "and the row opens onto the marker saying what was dropped: {expanded}"
    );
}

#[test]
fn a_live_reasoning_block_that_settles_empty_loses_the_row_it_was_streaming_in() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let (mut snapshot, activity_id) =
        reasoning_activity_session(workspace.path(), ActivityStatus::Active, None, "", None);
    snapshot.session.id = session_id;
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning block has only just started");
    assert!(
        rendered_application_rows_at(&application, 72, 24)
            .join("\n")
            .contains("Thinking"),
        "the reader watching the Turn sees that it is thinking"
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: next_revision,
                changes: vec![SessionChange::ReasoningStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(276),
                }],
            },
        )))
        .expect("project the Reasoning block settling with nothing to show");

    assert_reasoning_shows_nothing(
        &rendered_application_rows_at(&application, 72, 24).join("\n"),
        "the block settled empty, so the row it was streaming in goes with it",
    );
}

#[test]
fn a_reasoning_block_the_provider_only_titled_still_renders_its_row() {
    let workspace = workspace_dir();
    let (snapshot, _) = reasoning_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        Some("Inspecting the seam"),
        "",
        Some(4_200),
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a titled but wordless Reasoning block");

    let rows = rendered_application_rows_at(&application, 72, 24);
    assert_eq!(
        rows[rendered_row(&rows, "Thought: Inspecting the seam")].trim_end(),
        "    ✓ Thought: Inspecting the seam · 4s",
        "a title is something to say, so the block keeps its row"
    );
}

#[test]
fn a_reasoning_block_still_running_renders_before_the_provider_describes_it() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        reasoning_activity_session(workspace.path(), ActivityStatus::Active, None, "", None);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning block has only just started");

    let rows = rendered_application_rows_at(&application, 72, 24);
    assert_eq!(
        rows[rendered_row(&rows, "Thinking")].trim_end(),
        "    ⠋ Thinking",
        "a block that has not settled is live progress, so it keeps its row"
    );
}

/// A Session whose single Activity is a Subagent row, so a test can drive one
/// delegation's presentation without competing transcript content.
fn subagent_activity_session(
    workspace: &std::path::Path,
    status: ActivityStatus,
    description: &str,
    duration_ms: Option<u64>,
) -> (suru::protocol::SessionSnapshot, ActivityId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the mapping",
        workspace,
    );
    let turn_id = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, turn_id);
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Subagent {
        id: activity_id,
        turn_id,
        status,
        name: "Explore".to_owned(),
        description: description.to_owned(),
        model: None,
        session_id: SessionId::new(),
        duration_ms,
    };
    (snapshot, activity_id)
}

#[test]
fn subagent_rows_render_working_settled_failed_and_stopped_states() {
    let workspace = workspace_dir();
    let cases = [
        (
            ActivityStatus::Active,
            None,
            "⠋ Subagent: Explore: Map the provider seams",
            Color::Cyan,
        ),
        (
            ActivityStatus::Completed,
            Some(12_000),
            "✓ Subagent: Explore: Map the provider seams · 12s",
            Color::DarkGray,
        ),
        (
            ActivityStatus::Failed,
            Some(3_000),
            "× Subagent: Explore: Map the provider seams · 3s",
            Color::Red,
        ),
        // Stopped on request wears the interrupted Turn's own face, so a stop
        // reads as a stop rather than as the Subagent going wrong.
        (
            ActivityStatus::Interrupted,
            Some(8_000),
            "× Subagent: Explore: Map the provider seams · 8s",
            Color::Yellow,
        ),
    ];

    for (status, duration_ms, heading, color) in cases {
        let (snapshot, _) = subagent_activity_session(
            workspace.path(),
            status,
            "Map the provider seams",
            duration_ms,
        );
        let mut application = connected_application(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach Session with a Subagent Activity");

        let buffer = rendered_application_buffer(&application, 80, 22);
        let text = buffer_rows(&buffer).join("\n");
        assert!(
            text.contains(heading),
            "the Subagent row reads {heading:?}:\n{text}"
        );
        assert_eq!(text_cell(&buffer, heading).fg, color);
    }
}

#[test]
fn a_working_subagent_row_settles_in_place_when_its_outcome_arrives() {
    let workspace = workspace_dir();
    let (snapshot, activity_id) = subagent_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        "Map the provider seams",
        None,
    );
    let session_id = snapshot.session.id;
    let revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a working Subagent");

    let working = rendered_application_rows_at(&application, 80, 22).join("\n");
    assert!(
        working.contains("⠋ Subagent: Explore: Map the provider seams"),
        "a working Subagent wears the Marker's Spinner: {working}"
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::SubagentStatusChanged {
                    activity_id,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(72_000),
                }],
            },
        )))
        .expect("settle the Subagent");

    let settled_rows = rendered_application_rows_at(&application, 80, 22);
    assert_eq!(
        settled_rows[rendered_row(&settled_rows, "Explore: Map the provider seams")].trim_end(),
        "    ✓ Subagent: Explore: Map the provider seams · 1m 12s",
        "the settled row swaps its Spinner for the outcome glyph and states its duration"
    );
}

#[test]
fn a_subagent_asked_without_a_description_heads_with_its_prefixed_name() {
    let workspace = workspace_dir();
    let (snapshot, _) =
        subagent_activity_session(workspace.path(), ActivityStatus::Active, "", None);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with an undescribed Subagent");

    let rows = rendered_application_rows_at(&application, 80, 22);
    assert_eq!(
        rows[rendered_row(&rows, "⠋ Subagent: Explore")].trim_end(),
        "    ⠋ Subagent: Explore",
        "no separator trails a name with nothing to separate it from"
    );
}

#[test]
fn an_active_command_starts_folded_and_settles_in_place() {
    let workspace = workspace_dir();
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
        streaming.contains("⠋ cargo test"),
        "a streaming command starts in its one-line shape: {streaming}"
    );
    assert!(
        !streaming.contains("output line") && !streaming.contains("… +"),
        "the default keeps a streaming command's output folded: {streaming}"
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
        settled.contains("✓ cargo test"),
        "the settled command keeps its row: {settled}"
    );
    assert!(
        !settled.contains("output line") && !settled.contains("… +"),
        "a successful command settles into its single folded row: {settled}"
    );
}

#[test]
fn an_active_command_starts_folded_then_auto_promotes_under_the_expanded_opening_posture() {
    let workspace = workspace_dir();
    let (mut snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        &numbered_output(12),
        false,
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;

    let mut application = session_opened_under(
        workspace.path(),
        EffectiveSettings {
            transcript: TranscriptSettings {
                default_fold_posture: FoldPosture::Expanded,
                command_auto_expand: CommandAutoExpand::AfterMillis(0),
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &[
            "transcript.defaultFoldPosture",
            "transcript.commandAutoExpand",
        ],
        snapshot,
    );
    let rendered = rendered_application_rows_at(&application, 60, 24).join("\n");

    assert!(rendered.contains("⠋ cargo test"), "{rendered}");
    assert!(
        !rendered.contains("output line") && !rendered.contains("… +"),
        "every Active command begins Folded, independently of the settled-entry posture: {rendered}"
    );

    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("cross the zero-millisecond auto-expansion threshold");
    let promoted = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        promoted.contains("… +9 lines") && promoted.contains("output line 12"),
        "the numeric auto-expansion Setting still promotes under the expanded posture: {promoted}"
    );
}

#[test]
fn an_active_command_is_promoted_to_its_live_tail_after_the_configured_latency() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) = command_activity_session(
        workspace.path(),
        ActivityStatus::Active,
        &numbered_output(12),
        false,
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let session_id = snapshot.session.id;
    let revision = snapshot.revision;
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let observed_elapsed_ms = Arc::clone(&elapsed_ms);
    let origin = Instant::now();
    let mut application = Application::new(workspace.path(), Default::default())
        .with_presentation_clock(move || {
            origin + Duration::from_millis(observed_elapsed_ms.load(Ordering::Relaxed))
        });
    deliver_settings(
        &mut application,
        EffectiveSettings {
            transcript: TranscriptSettings {
                command_auto_expand: CommandAutoExpand::AfterMillis(500),
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["transcript.commandAutoExpand"],
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a streaming command");

    elapsed_ms.store(499, Ordering::Relaxed);
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("advance the presentation tick before the threshold");
    let before = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        !before.contains("output line") && !before.contains("… +"),
        "the command stays Folded before the configured latency: {before}"
    );

    elapsed_ms.store(500, Ordering::Relaxed);
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("advance the presentation tick at the threshold");
    let promoted = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        promoted.contains("… +9 lines")
            && promoted.contains("output line 10")
            && promoted.contains("output line 12"),
        "the command grows into the three-row live-tail view at the threshold: {promoted}"
    );
    assert!(
        !promoted.contains("output line 9"),
        "promotion reveals the tail rather than the whole stream: {promoted}"
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
        .expect("settle the promoted command");
    let settled = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        settled.contains("✓ cargo test")
            && !settled.contains("output line")
            && !settled.contains("… +"),
        "an automatic promotion ends with the command and success folds at once: {settled}"
    );
}

#[test]
fn a_saturated_live_command_tail_keeps_its_height_as_the_latest_line_wraps() {
    let workspace = workspace_dir();
    let output = format!("{}\n{}", "Z".repeat(80), "Z".repeat(80));
    let (mut snapshot, activity_id) =
        command_activity_session(workspace.path(), ActivityStatus::Active, &output, false);
    let session_id = snapshot.session.id;
    let revision = snapshot.revision;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let mut application = connected_application(workspace.path());
    deliver_settings(
        &mut application,
        EffectiveSettings {
            transcript: TranscriptSettings {
                command_auto_expand: CommandAutoExpand::AfterMillis(0),
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &["transcript.commandAutoExpand"],
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with wrapping streaming output");
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("promote the command into its live tail");

    let live_tail_height = |application: &Application| {
        rendered_application_rows_at(application, 60, 24)
            .iter()
            // The Session header spells the Workspace path, and a temp
            // directory's random name spells a Z often enough to be counted as
            // a row of output unless the header is left out of the count.
            .skip(1)
            .filter(|row| row.contains('Z') || row.contains("… +"))
            .count()
    };
    let before = live_tail_height(&application);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: "\nZ".to_owned(),
                }],
            },
        )))
        .expect("append one short source line");
    let with_short_line = live_tail_height(&application);

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 2),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: "Z".repeat(79),
                }],
            },
        )))
        .expect("wrap the newest source line");
    let after_wrap = live_tail_height(&application);

    assert_eq!(
        [before, with_short_line, after_wrap],
        [4; 3],
        "the marker plus a saturated three-row live tail keeps one height"
    );
}

#[test]
fn interrupting_a_turn_lands_the_watched_command_in_its_peek() {
    let workspace = workspace_dir();
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
    let folded_rows = rendered_application_rows_at(&application, 60, 24);
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "⠋ cargo test") as u16,
    )
    .expect("open the running command by hand");
    assert!(
        rendered_application_rows_at(&application, 60, 24)
            .join("\n")
            .contains("output line 1"),
        "manual disclosure reveals the command the reader is watching"
    );

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
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
        .expect("settle the interrupted command");

    let interrupted = rendered_application_rows_at(&application, 60, 40).join("\n");
    assert!(
        interrupted.contains("… +6 lines") && interrupted.contains("output line 12"),
        "the tail the reader was watching stays visible as a Peek instead of folding away: {interrupted}"
    );
    assert!(
        !interrupted.contains("output line 6"),
        "the interrupt opens the Peek, not the whole stream: {interrupted}"
    );
}

#[test]
fn expanding_a_capped_command_reveals_everything_stored_before_the_truncation_marker() {
    let workspace = workspace_dir();
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
        !folded.contains("[output truncated]"),
        "the single folded row keeps even the truncation marker back: {folded}"
    );

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("open the capped entry's Peek");
    let peek_rows = rendered_application_rows_at(&application, 60, 30);
    let peek = peek_rows.join("\n");
    assert!(
        peek.contains("… +6 lines") && peek.contains("[output truncated]"),
        "one entry carries both a Fold and a Truncation: {peek}"
    );

    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "… +6 lines") as u16,
    )
    .expect("open the capped entry the rest of the way");

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
    let workspace = workspace_dir();
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

    left_click_at(
        &mut reader,
        rendered_row(&folded_rows, "✓ cargo test") as u16,
    )
    .expect("one client opens the entry's Peek");

    assert!(
        rendered_application_rows_at(&reader, 60, 24)
            .join("\n")
            .contains("… +6 lines"),
        "the client that clicked sees the Peek"
    );
    let observed = rendered_application_rows_at(&observer, 60, 24).join("\n");
    assert!(
        !observed.contains("output line") && !observed.contains("… +"),
        "a Fold is client-local view state and never reaches another client: {observed}"
    );
}

#[test]
fn clicking_an_entry_that_hides_nothing_records_no_fold_for_its_later_output() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) =
        command_activity_session(workspace.path(), ActivityStatus::Active, "", false);
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
        !rows.join("\n").contains("output line") && !rows.join("\n").contains("… +"),
        "the fixture starts with no content to hide"
    );

    left_click_at(&mut application, rendered_row(&rows, "⠋ cargo test") as u16)
        .expect("click an entry that hides nothing");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: numbered_output(12),
                }],
            },
        )))
        .expect("stream more output than the Fold budget");

    let grown = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        !grown.contains("output line") && !grown.contains("… +"),
        "a click on an entry with nothing to hide leaves the default Fold in charge: {grown}"
    );
}

#[test]
fn clicking_an_empty_active_row_under_the_expanded_posture_does_not_block_auto_promotion() {
    let workspace = workspace_dir();
    let (mut snapshot, activity_id) =
        command_activity_session(workspace.path(), ActivityStatus::Active, "", false);
    let session_id = snapshot.session.id;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    let revision = snapshot.revision;
    let mut application = session_opened_under(
        workspace.path(),
        EffectiveSettings {
            transcript: TranscriptSettings {
                default_fold_posture: FoldPosture::Expanded,
                command_auto_expand: CommandAutoExpand::AfterMillis(0),
                ..TranscriptSettings::default()
            },
            ..EffectiveSettings::default()
        },
        &[
            "transcript.defaultFoldPosture",
            "transcript.commandAutoExpand",
        ],
        snapshot,
    );
    let empty = rendered_application_rows_at(&application, 60, 24);

    left_click_at(
        &mut application,
        rendered_row(&empty, "⠋ cargo test") as u16,
    )
    .expect("click the empty Active row");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::CommandOutputAppended {
                    activity_id,
                    content: numbered_output(12),
                }],
            },
        )))
        .expect("stream output after the empty-row click");
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .expect("cross the zero-millisecond auto-expansion threshold");

    let promoted = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        promoted.contains("… +9 lines") && promoted.contains("output line 12"),
        "an empty-row click records no manual Fold and leaves automatic promotion free: {promoted}"
    );
}

#[test]
fn clicks_do_not_reach_the_transcript_while_a_picker_covers_it() {
    let workspace = workspace_dir();
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
    let command_row = rendered_row(&folded_rows, "✓ cargo test") as u16;

    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("open the Session picker over the transcript");
    }
    left_click_at(&mut application, command_row)
        .expect("click while the picker covers the transcript");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close the Session picker");

    let after = rendered_application_rows_at(&application, 60, 24).join("\n");
    assert!(
        !after.contains("output line"),
        "the entry beneath the picker keeps its Fold: {after}"
    );
}

/// One transcript entry in a Group scenario, so a test states the shape of the
/// run it drives instead of assembling Activities by hand. Commands are
/// numbered `command 1`, `command 2`, … in transcript order so member rows are
/// assertable by name.
#[derive(Clone, Copy)]
enum RunEntry {
    Command(ActivityStatus, Option<i32>),
    AgentMessage(&'static str),
    UserMessage(&'static str),
    Reasoning(ReasoningBlock),
    FileChange,
    Status(&'static str),
    Error(&'static str),
}

/// One Reasoning block a run fixture holds. Every property is spelled out
/// because a Reasoning Group reads every one of them off its members: the
/// heading its row leads with, the prose its expansion opens onto, whether the
/// cap cut that prose short, how the block settled, and how long it took.
#[derive(Clone, Copy)]
struct ReasoningBlock {
    status: ActivityStatus,
    title: Option<&'static str>,
    content: &'static str,
    content_truncated: bool,
    duration_ms: Option<u64>,
}

impl ReasoningBlock {
    /// A block that settled with a heading, prose, and a duration — the shape
    /// a described section of thinking arrives in.
    const fn thought(title: &'static str, content: &'static str, duration_ms: u64) -> Self {
        Self {
            status: ActivityStatus::Completed,
            title: Some(title),
            content,
            content_truncated: false,
            duration_ms: Some(duration_ms),
        }
    }

    /// A block that settled with prose the Provider never headed.
    const fn untitled(content: &'static str, duration_ms: u64) -> Self {
        Self {
            status: ActivityStatus::Completed,
            title: None,
            content,
            content_truncated: false,
            duration_ms: Some(duration_ms),
        }
    }

    /// A block stored before Suru recorded Reasoning durations, so it has a
    /// heading and prose but nothing to contribute to a summed duration.
    const fn untimed(title: &'static str, content: &'static str) -> Self {
        Self {
            status: ActivityStatus::Completed,
            title: Some(title),
            content,
            content_truncated: false,
            duration_ms: None,
        }
    }

    /// A block still streaming its prose under a heading the Provider led the
    /// section with.
    const fn streaming(title: &'static str, content: &'static str) -> Self {
        Self {
            status: ActivityStatus::Active,
            title: Some(title),
            content,
            content_truncated: false,
            duration_ms: None,
        }
    }

    /// A block still streaming a section the Provider has not headed, which is
    /// what a live row has to hold a title across.
    const fn streaming_untitled(content: &'static str) -> Self {
        Self {
            status: ActivityStatus::Active,
            title: None,
            content,
            content_truncated: false,
            duration_ms: None,
        }
    }

    /// A block a Turn cut short partway through its prose.
    const fn interrupted(content: &'static str) -> Self {
        Self {
            status: ActivityStatus::Failed,
            title: None,
            content,
            content_truncated: false,
            duration_ms: None,
        }
    }

    /// The same block, with the cap having cut its stored prose short.
    const fn capped(self) -> Self {
        Self {
            content_truncated: true,
            ..self
        }
    }

    /// A block the Provider settled without ever describing: no title, no
    /// content, and only the duration it spent arriving at nothing a reader
    /// could see.
    const EMPTY: Self = Self {
        status: ActivityStatus::Completed,
        title: None,
        content: "",
        content_truncated: false,
        duration_ms: Some(276),
    };
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
    set_turn_in_flight(&mut snapshot, turn_id);
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
                    skill_invocations: Vec::new(),
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
            RunEntry::Reasoning(block) => Activity::Reasoning {
                id: ActivityId::new(),
                turn_id,
                status: block.status,
                title: block.title.map(ToOwned::to_owned),
                content: block.content.to_owned(),
                content_truncated: block.content_truncated,
                duration_ms: block.duration_ms,
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
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &[SUCCESSFUL_COMMAND]);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with one successful command");

    let rows = rendered_application_rows_at(&application, 80, 18);
    let rendered = rows.join("\n");

    assert!(
        rendered.contains("✓ command 1"),
        "a run of one renders the ordinary command row: {rendered}"
    );
    assert!(
        !rendered.contains("output of command 1"),
        "the ordinary row folds to its single row like any settled command: {rendered}"
    );
    assert!(
        !rendered.contains("Ran 1 command"),
        "grouping never adds a layer where it saves nothing: {rendered}"
    );
}

#[test]
fn every_other_entry_kind_and_unsuccessful_commands_break_a_command_run() {
    let workspace = workspace_dir();
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
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Weighing options",
                "Weighed the options.",
                4_000,
            )),
            "Thought: Weighing options",
        ),
        (RunEntry::FileChange, "✓ Created src/new.rs"),
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
        let mut application = client_showing_reasoning(workspace.path());
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
fn an_empty_reasoning_block_neither_breaks_a_command_run_nor_leaves_a_gap() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let with_empty_blocks = command_run_snapshot(
        session_id,
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::EMPTY),
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::EMPTY),
            RunEntry::AgentMessage("The workflow is green."),
        ],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(with_empty_blocks))
        .expect("attach a Session whose command run is interleaved with empty Reasoning");
    let rows = rendered_application_rows_at(&application, 80, 24);
    assert!(
        rows.join("\n").contains("✓ Ran 2 commands"),
        "an invisible block neither joins nor ends the run around it: {}",
        rows.join("\n")
    );

    let without_empty_blocks = command_run_snapshot(
        session_id,
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::AgentMessage("The workflow is green."),
        ],
    );
    let mut without = connected_application(workspace.path());
    without
        .handle_event(ApplicationEvent::SessionAttached(without_empty_blocks))
        .expect("attach the same Session without the empty Reasoning blocks");

    assert_eq!(
        rows,
        rendered_application_rows_at(&without, 80, 24),
        "a Transcript reads exactly as it would had the empty blocks never happened, \
         down to the rows of air between its entries"
    );
}

#[test]
fn hidden_reasoning_neither_breaks_a_command_run_nor_leaves_a_gap() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let with_reasoning = command_run_snapshot(
        session_id,
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Weighing options",
                "The second command settles it.",
                4_000,
            )),
            SUCCESSFUL_COMMAND,
            RunEntry::AgentMessage("The workflow is green."),
        ],
    );
    let hiding = session_opened_under(
        workspace.path(),
        settings_with_reasoning(ReasoningVisibility::Hidden),
        &["transcript.reasoningVisibility"],
        with_reasoning,
    );
    let rows = rendered_application_rows_at(&hiding, 80, 24);
    assert!(
        rows.join("\n").contains("✓ Ran 2 commands"),
        "a hidden block neither joins nor ends the run around it: {}",
        rows.join("\n")
    );

    let without_reasoning = command_run_snapshot(
        session_id,
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::AgentMessage("The workflow is green."),
        ],
    );
    let mut without = connected_application(workspace.path());
    without
        .handle_event(ApplicationEvent::SessionAttached(without_reasoning))
        .expect("attach the same Session without the Reasoning between its commands");

    assert_eq!(
        rows,
        rendered_application_rows_at(&without, 80, 24),
        "a Transcript with Reasoning hidden reads exactly as it would had the \
         thinking never happened, down to the rows of air between its entries"
    );
}

#[test]
fn interrupting_a_turn_does_not_split_a_command_group_at_hidden_reasoning() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = command_run_snapshot(
        session_id,
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Checking the result",
                "The second command can run.",
                4_000,
            )),
            SUCCESSFUL_COMMAND,
        ],
    );
    let interrupted_turn = snapshot.turns[0].id;
    let revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a live Turn whose command run crosses hidden Reasoning");

    let live = rendered_application_rows_at(&application, 80, 24).join("\n");
    assert!(
        live.contains("✓ Ran 2 commands"),
        "the live Turn starts with its command Group intact: {live}"
    );

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![SessionChange::TurnStatusChanged {
                    turn_id: interrupted_turn,
                    status: TurnStatus::Interrupted,
                    settled_at: None,
                }],
            },
        )))
        .expect("settle the interrupted Turn");

    let interrupted = rendered_application_rows_at(&application, 80, 24).join("\n");
    assert!(
        interrupted.contains("✓ Ran 2 commands"),
        "opening the interrupted Turn preserves the command Group: {interrupted}"
    );
    for member in ["command 1", "command 2"] {
        assert!(
            !interrupted.contains(member),
            "the collapsed Group still hides {member:?}: {interrupted}"
        );
    }
}

#[test]
fn an_active_command_grows_beneath_the_existing_group_while_the_turn_runs() {
    let workspace = workspace_dir();
    let mut snapshot = live_command_run_snapshot(SessionId::new(), workspace.path());
    if let Activity::Command { output, .. } = &mut snapshot.activities[2] {
        *output = numbered_output(12);
    }
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");

    let rows = rendered_application_rows_at(&application, 80, 18);
    let rendered = rows.join("\n");

    assert!(
        rendered.contains("✓ Ran 2 commands"),
        "the settled run groups while the Turn is still active: {rendered}"
    );
    assert!(
        !rendered.contains("command 1") && !rendered.contains("command 2"),
        "no member row escapes the Group: {rendered}"
    );
    assert!(
        rendered.contains("⠋ command 3"),
        "the running command stays visible beneath the Group: {rendered}"
    );
    let running = &rows[rendered_row(&rows, "⠋ command 3")];
    assert!(
        running.starts_with("      ⠋ command 3"),
        "the live member sits in the Group's member gutter: {running:?}"
    );
    assert!(
        !rendered.contains("output line") && !rendered.contains("… +"),
        "the live member starts Folded under the default Setting: {rendered}"
    );
}

#[test]
fn groups_have_no_size_cap() {
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
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
fn an_interrupted_command_settles_as_a_standalone_failed_row_in_its_peek() {
    let workspace = workspace_dir();
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
    assert!(
        rendered.contains("… +6 lines") && rendered.contains("output line 12"),
        "the tail the reader was watching stays visible as a Peek: {rendered}"
    );
    assert!(
        !rendered.contains("output line 6"),
        "the Peek keeps the head behind its marker: {rendered}"
    );
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
    let workspace = workspace_dir();
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

    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
    )
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
    assert!(
        !rendered.contains("output line") && !rendered.contains("… +"),
        "a member renders in its default Fold presentation — a single row: {rendered}"
    );
}

#[test]
fn a_members_fold_toggles_independently_within_an_expanded_group() {
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
    )
    .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 36);
    let expanded = expanded_rows.join("\n");
    assert!(
        !expanded.contains("first line") && !expanded.contains("second line"),
        "both members start out as single folded rows: {expanded}"
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "✓ command 1") as u16,
    )
    .expect("open one member's Peek");

    let rows = rendered_application_rows_at(&application, 80, 36);
    let rendered = rows.join("\n");
    for line in 7..=12 {
        assert!(
            rendered.contains(&format!("first line {line}")),
            "the clicked member opens to its Peek: {rendered}"
        );
    }
    assert!(
        rendered.contains("… +6 lines") && !rendered.contains("second line"),
        "the sibling member's Fold is untouched: {rendered}"
    );
    assert!(
        rendered.contains("✓ Ran 2 commands"),
        "a member's Fold never collapses the Group: {rendered}"
    );

    left_click_at(&mut application, rendered_row(&rows, "✓ command 1") as u16)
        .expect("fold the member back from its header");
    let refolded = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert!(
        !refolded.contains("first line") && refolded.contains("✓ Ran 2 commands"),
        "the member folds back while the Group stays expanded: {refolded}"
    );
}

#[test]
fn a_members_fold_override_survives_collapse_and_re_expansion() {
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
    )
    .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "✓ command 1") as u16,
    )
    .expect("open the member's Peek");
    let peek_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "… +6 lines") as u16,
    )
    .expect("open the member's Fold the rest of the way");
    let unfolded_rows = rendered_application_rows_at(&application, 80, 36);

    left_click_at(
        &mut application,
        rendered_row(&unfolded_rows, "Ran 2 commands") as u16,
    )
    .expect("collapse the Group over the unfolded member");
    let recollapsed_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&recollapsed_rows, "Ran 2 commands") as u16,
    )
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
    let workspace = workspace_dir();
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
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
    )
    .expect("expand the Group");
    let expanded_rows = rendered_application_rows_at(&application, 80, 30);

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "✓ command 2") as u16,
    )
    .expect("click a member row below the header");
    let still_expanded = rendered_application_rows_at(&application, 80, 30).join("\n");
    assert!(
        still_expanded.contains("✓ command 2"),
        "a click below the header leaves the Group expanded: {still_expanded}"
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "Ran 3 commands") as u16,
    )
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
    let workspace = workspace_dir();
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

    left_click_at(
        &mut reader,
        rendered_row(&collapsed_rows, "Ran 3 commands") as u16,
    )
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
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = live_command_run_snapshot(session_id, workspace.path());
    let running_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a running command after a run");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 30);
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
    )
    .expect("expand the Group while the Turn runs");
    let expanded_rows = rendered_application_rows_at(&application, 80, 30);
    let running = &expanded_rows[rendered_row(&expanded_rows, "⠋ command 3")];
    assert!(
        running.starts_with("      ⠋ command 3"),
        "the running command is already a member of the expanded Group: {running:?}"
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
        !rendered.contains("⠋ command 3"),
        "the standalone running row is gone: {rendered}"
    );
}

#[test]
fn toggling_the_group_posture_flips_every_group_and_clears_per_group_overrides() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Command(ActivityStatus::Failed, Some(2)),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
        ],
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with two Groups");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
    )
    .expect("expand the first Group by hand");

    press_leader_chord(&mut application, 'g');
    let expanded = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert_eq!(
        expanded.matches("Ran 2 commands").count(),
        2,
        "both Group headers stay when the posture expands: {expanded}"
    );
    for member in ["✓ command 1", "✓ command 2", "✓ command 4", "✓ command 5"] {
        assert!(
            expanded.contains(member),
            "the expanded posture shows every Group's members, missing {member:?}: {expanded}"
        );
    }

    press_leader_chord(&mut application, 'g');
    let recollapsed = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert_eq!(
        recollapsed.matches("Ran 2 commands").count(),
        2,
        "both Groups collapse back to their single rows: {recollapsed}"
    );
    for member in ["command 1", "command 2", "command 4", "command 5"] {
        assert!(
            !recollapsed.contains(member),
            "flipping back collapses even the Group the reader expanded by hand, \
             but {member:?} rendered: {recollapsed}"
        );
    }
}

#[test]
fn each_disclosure_toggle_leaves_the_other_axis_untouched() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Command(ActivityStatus::Failed, Some(2)),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
        ],
    );
    set_member_output(&mut snapshot, "command 1", prefixed_output("member", 12));
    set_member_output(&mut snapshot, "command 3", prefixed_output("breaker", 12));
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with two Groups and long outputs");
    let collapsed_rows = rendered_application_rows_at(&application, 80, 50);
    left_click_at(
        &mut application,
        rendered_row(&collapsed_rows, "Ran 2 commands") as u16,
    )
    .expect("expand the first Group by hand");
    let member_rows = rendered_application_rows_at(&application, 80, 50);
    left_click_at(
        &mut application,
        rendered_row(&member_rows, "✓ command 1") as u16,
    )
    .expect("open the first member's Peek by hand");
    let peek_rows = rendered_application_rows_at(&application, 80, 50);
    left_click_at(
        &mut application,
        rendered_row(&peek_rows, "… +6 lines") as u16,
    )
    .expect("open the first member's Fold the rest of the way");

    press_leader_chord(&mut application, 'g');
    let groups_expanded = rendered_application_rows_at(&application, 80, 50).join("\n");
    assert!(
        groups_expanded.contains("member line 6"),
        "the Groups toggle leaves the member's Fold override in place: {groups_expanded}"
    );
    assert!(
        !groups_expanded.contains("breaker line 6"),
        "the Groups toggle leaves an untouched Fold folded: {groups_expanded}"
    );

    press_leader_chord(&mut application, 'g');
    let collapsed_again = rendered_application_rows_at(&application, 80, 50);
    left_click_at(
        &mut application,
        rendered_row(&collapsed_again, "Ran 2 commands") as u16,
    )
    .expect("expand the first Group by hand again");

    press_leader_chord(&mut application, 'f');
    let folds_expanded = rendered_application_rows_at(&application, 80, 50).join("\n");
    assert!(
        folds_expanded.contains("breaker line 6"),
        "the Folds toggle expands every Fold: {folds_expanded}"
    );
    assert!(
        folds_expanded.contains("✓ command 1"),
        "the Folds toggle leaves the hand-expanded Group expanded: {folds_expanded}"
    );
    assert!(
        !folds_expanded.contains("command 4"),
        "the Folds toggle leaves the collapsed Group collapsed: {folds_expanded}"
    );
    assert_eq!(
        folds_expanded.matches("Ran 2 commands").count(),
        2,
        "both Group headers survive the Folds toggle: {folds_expanded}"
    );
}

/// The run of described thinking every Reasoning Group test starts from:
/// three titled sections whose durations sum to a span the humanizer reports
/// in minutes, so a marker that summed them wrongly reads wrongly.
const INSPECTING: ReasoningBlock =
    ReasoningBlock::thought("Inspecting the seam", "Read the projection.", 30_000);
const WEIGHING: ReasoningBlock =
    ReasoningBlock::thought("Weighing the options", "Weighed the options.", 2_000);
const SETTLING: ReasoningBlock =
    ReasoningBlock::thought("Settling on the plan", "Settled on the plan.", 40_000);

const REASONING_RUN: [RunEntry; 3] = [
    RunEntry::Reasoning(INSPECTING),
    RunEntry::Reasoning(WEIGHING),
    RunEntry::Reasoning(SETTLING),
];

/// The prose the run's members hold, read off the members themselves so a
/// test asserting it is hidden cannot drift from what they carry.
const REASONING_RUN_PROSE: [&str; 3] = [INSPECTING.content, WEIGHING.content, SETTLING.content];

/// The shape a Reasoning run has mid-Turn: two settled sections and a third
/// still streaming, inside a Session and Turn still marked Active. The settled
/// members are short-titled so a live row's held title is assertable against
/// the section it was held from.
fn live_reasoning_run_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
    streaming: ReasoningBlock,
) -> suru::protocol::SessionSnapshot {
    let mut snapshot = command_run_snapshot(
        session_id,
        workspace,
        &[
            RunEntry::Reasoning(ReasoningBlock::thought("Reading", "Read the plan.", 300)),
            RunEntry::Reasoning(ReasoningBlock::thought("Weighing", "Weighed it.", 400)),
            RunEntry::Reasoning(streaming),
        ],
    );
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    snapshot
}

#[test]
fn hidden_reasoning_leaves_a_live_run_nothing_to_stream_into() {
    let workspace = workspace_dir();
    let snapshot = live_reasoning_run_snapshot(
        SessionId::new(),
        workspace.path(),
        ReasoningBlock::streaming("Settling on the plan", "Still weighing it."),
    );
    let application = session_opened_under(
        workspace.path(),
        settings_with_reasoning(ReasoningVisibility::Hidden),
        &["transcript.reasoningVisibility"],
        snapshot,
    );

    let rendered = rendered_application_rows_at(&application, 80, 24).join("\n");
    for absent in [
        "Thinking",
        "Thought",
        "Settling on the plan",
        "Still weighing it.",
    ] {
        assert!(
            !rendered.contains(absent),
            "a Group forming live has nothing to form from once Reasoning is hidden, \
             but {absent:?} rendered: {rendered}"
        );
    }
}

#[test]
fn a_run_of_settled_reasoning_blocks_collapses_to_one_thought_row() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &REASONING_RUN);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of settled Reasoning blocks");

    let buffer = rendered_application_buffer(&application, 80, 18);
    let rows = buffer_rows(&buffer);
    let rendered = rows.join("\n");

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought: Settling on the plan · 3 steps · 1m 12s",
        "a run of three settled Reasoning blocks is one row headed by the latest \
         member's title, counting its members and summing their durations: {rendered}"
    );
    assert_eq!(
        rendered.matches("Thought").count(),
        1,
        "the run reads as one row, not one row per member: {rendered}"
    );
    for hidden in ["Inspecting the seam", "Weighing the options"] {
        assert!(
            !rendered.contains(hidden),
            "a collapsed Group shows only the latest member's title, but {hidden:?} \
             rendered: {rendered}"
        );
    }
    for hidden in REASONING_RUN_PROSE {
        assert!(
            !rendered.contains(hidden),
            "a collapsed Group hides its members' prose, but {hidden:?} rendered: {rendered}"
        );
    }
    let affordance = text_cell(&buffer, "3 steps");
    assert_eq!(
        affordance.fg,
        Color::Blue,
        "the step count is the expand affordance, styled action-primary"
    );
}

#[test]
fn a_reasoning_group_marker_drops_the_description_when_its_latest_member_has_none() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Inspecting the seam",
                "Read the projection.",
                30_000,
            )),
            RunEntry::Reasoning(ReasoningBlock::untitled("Kept reading.", 2_000)),
        ],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose latest Reasoning block was never titled");

    let rows = rendered_application_rows_at(&application, 80, 18);

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought · 2 steps · 32s",
        "the marker says where the thinking ended up, so an untitled latest member \
         leaves it with no description to give: {}",
        rows.join("\n")
    );
}

#[test]
fn a_run_of_one_visible_reasoning_block_keeps_the_presentation_it_has_alone() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::Reasoning(ReasoningBlock::thought(
            "Inspecting the seam",
            "Read the projection.",
            4_000,
        ))],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with one settled Reasoning block");

    let rows = rendered_application_rows_at(&application, 80, 18);

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought: Inspecting the seam · 4s · +1 lines",
        "a lone block keeps its own header-and-fold row, counting the lines it hides \
         rather than steps it does not have: {}",
        rows.join("\n")
    );
}

#[test]
fn clicking_a_reasoning_group_opens_onto_every_members_prose_and_folds_back() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &REASONING_RUN);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of settled Reasoning blocks");

    let collapsed = rendered_application_rows_at(&application, 80, 24);
    left_click_at(&mut application, rendered_row(&collapsed, "Thought") as u16)
        .expect("expand the Reasoning Group");

    let buffer = rendered_application_buffer(&application, 80, 24);
    let expanded_rows = buffer_rows(&buffer);
    let expanded = expanded_rows.join("\n");
    let header = rendered_row(&expanded_rows, "Thought");
    assert_eq!(
        expanded_rows[header..header + 9]
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        [
            "    ✓ Thought: Settling on the plan · 3 steps · 1m 12s",
            "      Inspecting the seam",
            "      Read the projection.",
            "",
            "      Weighing the options",
            "      Weighed the options.",
            "",
            "      Settling on the plan",
            "      Settled on the plan.",
        ],
        "expanding opens straight onto every member's prose in the order it happened, \
         each section under the title that heads it, below the header the Group \
         re-collapses from: {expanded}"
    );
    for heading in ["Inspecting the seam", "Weighing the options"] {
        let cell = text_cell(&buffer, heading);
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "each member's title heads its own section in bold, but {heading:?} did not: \
             {expanded}"
        );
    }

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "Thought") as u16,
    )
    .expect("fold the Reasoning Group back");

    let refolded = rendered_application_rows_at(&application, 80, 24);
    assert_eq!(
        refolded, collapsed,
        "folding back returns the Transcript to the single marker it opened from"
    );
}

#[test]
fn clicking_an_expanded_reasoning_groups_prose_leaves_it_open() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &REASONING_RUN);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with a run of settled Reasoning blocks");

    let collapsed = rendered_application_rows_at(&application, 80, 24);
    left_click_at(&mut application, rendered_row(&collapsed, "Thought") as u16)
        .expect("expand the Reasoning Group");
    let expanded = rendered_application_rows_at(&application, 80, 24);
    left_click_at(
        &mut application,
        rendered_row(&expanded, "Read the projection.") as u16,
    )
    .expect("click the revealed prose");

    assert_eq!(
        rendered_application_rows_at(&application, 80, 24),
        expanded,
        "revealed prose is reading surface, not an affordance: only the header folds \
         the Group back"
    );
}

#[test]
fn an_interrupted_reasoning_block_ends_the_run_and_stands_outside_the_group() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::Reasoning(ReasoningBlock::thought("Reading", "Read the plan.", 300)),
            RunEntry::Reasoning(ReasoningBlock::thought("Weighing", "Weighed it.", 400)),
            RunEntry::Reasoning(ReasoningBlock::interrupted("Got halfway.")),
            RunEntry::Reasoning(ReasoningBlock::thought("Retrying", "Read it again.", 300)),
            RunEntry::Reasoning(ReasoningBlock::thought("Settling", "Settled on it.", 400)),
        ],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning run an interrupt cut in two");

    let rows = rendered_application_rows_at(&application, 80, 24);
    let rendered = rows.join("\n");

    assert_eq!(
        rendered.matches("· 2 steps · 700ms").count(),
        2,
        "the interrupted block ends the run, leaving one Group on either side: {rendered}"
    );
    assert!(
        rendered.contains("× Thinking interrupted"),
        "the interrupted block stands outside the Groups as its own row: {rendered}"
    );
    let interrupted = rendered_row(&rows, "Thinking interrupted");
    assert!(
        rendered_row(&rows, "Thought: Weighing") < interrupted
            && rows[interrupted + 1..]
                .iter()
                .any(|row| row.contains("Thought: Settling")),
        "presentation order is preserved: {rendered}"
    );
}

#[test]
fn an_empty_reasoning_block_neither_joins_a_reasoning_group_nor_counts_toward_it() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::Reasoning(ReasoningBlock::thought("Reading", "Read the plan.", 300)),
            RunEntry::Reasoning(ReasoningBlock::EMPTY),
            RunEntry::Reasoning(ReasoningBlock::thought("Settling", "Settled on it.", 400)),
        ],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning run is interleaved with an empty block");

    let rows = rendered_application_rows_at(&application, 80, 18);

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought: Settling · 2 steps · 700ms",
        "an invisible block neither ends the run nor counts toward the step count or \
         the duration the marker sums: {}",
        rows.join("\n")
    );
}

#[test]
fn every_other_visible_entry_kind_ends_a_reasoning_run() {
    let workspace = workspace_dir();
    let breakers: [(RunEntry, &str); 6] = [
        (
            RunEntry::AgentMessage("A breaking message"),
            "A breaking message",
        ),
        (
            RunEntry::UserMessage("A breaking user message"),
            "A breaking user message",
        ),
        (SUCCESSFUL_COMMAND, "✓ command 1"),
        (RunEntry::FileChange, "✓ Created src/new.rs"),
        (
            RunEntry::Status("Agent Selection changed"),
            "Agent Selection changed",
        ),
        (RunEntry::Error("Provider failed"), "Error: Provider failed"),
    ];
    for (breaker, visible) in breakers {
        let snapshot = command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[
                RunEntry::Reasoning(ReasoningBlock::thought("Reading", "Read the plan.", 300)),
                RunEntry::Reasoning(ReasoningBlock::thought("Weighing", "Weighed it.", 400)),
                breaker,
                RunEntry::Reasoning(ReasoningBlock::thought("Retrying", "Read it again.", 300)),
                RunEntry::Reasoning(ReasoningBlock::thought("Settling", "Settled on it.", 400)),
            ],
        );
        let mut application = client_showing_reasoning(workspace.path());
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .expect("attach a Session with a broken Reasoning run");

        let rows = rendered_application_rows_at(&application, 80, 24);
        let rendered = rows.join("\n");
        assert_eq!(
            rendered.matches("· 2 steps · 700ms").count(),
            2,
            "{visible:?} splits the run into two Groups: {rendered}"
        );
        let breaker_row = rendered_row(&rows, visible);
        assert!(
            rendered_row(&rows, "Thought: Weighing") < breaker_row
                && rows[breaker_row + 1..]
                    .iter()
                    .any(|row| row.contains("Thought: Settling")),
            "presentation order is preserved: {rendered}"
        );
    }
}

#[test]
fn a_reasoning_group_whose_members_were_never_timed_reports_no_duration() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::Reasoning(ReasoningBlock::untimed("Reading", "Read the plan.")),
            RunEntry::Reasoning(ReasoningBlock::untimed("Settling", "Settled on it.")),
        ],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Reasoning blocks predate recorded durations");

    let rows = rendered_application_rows_at(&application, 80, 18);

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought: Settling · 2 steps",
        "a run with no durations to sum states none, exactly as a lone block \
         without one does: {}",
        rows.join("\n")
    );
}

#[test]
fn an_expanded_reasoning_group_marks_the_member_whose_prose_the_cap_cut_short() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::Reasoning(ReasoningBlock::thought("Reading", "Read the plan.", 300).capped()),
            RunEntry::Reasoning(ReasoningBlock::thought("Settling", "Settled on it.", 400)),
        ],
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose first Reasoning block the cap cut short");

    let collapsed = rendered_application_rows_at(&application, 80, 18);
    left_click_at(&mut application, rendered_row(&collapsed, "Thought") as u16)
        .expect("expand the Reasoning Group");

    let rows = rendered_application_rows_at(&application, 80, 18);
    let header = rendered_row(&rows, "Thought");
    assert_eq!(
        rows[header..header + 6]
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        [
            "    ✓ Thought: Settling · 2 steps · 700ms",
            "      Reading",
            "      Read the plan.",
            "      [Reasoning truncated]",
            "",
            "      Settling",
        ],
        "a member the cap cut short ends in the marker that says so, inside the \
         Group as it does outside one: {}",
        rows.join("\n")
    );
}

#[test]
fn a_streaming_reasoning_block_joins_the_group_as_its_live_thinking_row() {
    let workspace = workspace_dir();
    let snapshot = live_reasoning_run_snapshot(
        SessionId::new(),
        workspace.path(),
        ReasoningBlock::streaming("Settling", "Settling on it."),
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session still thinking after a settled run");

    let rows = rendered_application_rows_at(&application, 80, 18);
    let rendered = rows.join("\n");

    assert_eq!(
        rows[rendered_row(&rows, "Thinking")].trim_end(),
        "    ⠋ Thinking: Settling",
        "a block belongs to its Group from the moment it starts, so the run stands \
         as one live row naming what it is thinking about: {rendered}"
    );
    assert!(
        !rendered.contains("Thought"),
        "the run has not settled, so no row in it speaks of thinking that finished: \
         {rendered}"
    );
    for hidden in ["Read the plan.", "Weighed it.", "Settling on it."] {
        assert!(
            !rendered.contains(hidden),
            "streaming Reasoning prose is hidden until the reader asks for it, but \
             {hidden:?} rendered: {rendered}"
        );
    }
}

#[test]
fn a_live_reasoning_row_holds_its_last_title_while_an_untitled_section_streams() {
    let workspace = workspace_dir();
    let snapshot = live_reasoning_run_snapshot(
        SessionId::new(),
        workspace.path(),
        ReasoningBlock::streaming_untitled("Still going."),
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose streaming section the Provider has not headed");

    let rows = rendered_application_rows_at(&application, 80, 18);

    assert_eq!(
        rows[rendered_row(&rows, "Thinking")].trim_end(),
        "    ⠋ Thinking: Weighing",
        "a live row names the most recent topic it knows, so an unheaded section \
         streams under the title before it rather than under none: {}",
        rows.join("\n")
    );
}

#[test]
fn clicking_the_live_reasoning_row_reveals_the_streaming_prose_and_hides_it_again() {
    let workspace = workspace_dir();
    let snapshot = live_reasoning_run_snapshot(
        SessionId::new(),
        workspace.path(),
        ReasoningBlock::streaming("Settling", "Settling on it."),
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session still thinking after a settled run");

    let collapsed = rendered_application_rows_at(&application, 80, 24);
    left_click_at(
        &mut application,
        rendered_row(&collapsed, "Thinking") as u16,
    )
    .expect("open the live Reasoning row");

    let expanded_rows = rendered_application_rows_at(&application, 80, 24);
    let header = rendered_row(&expanded_rows, "Thinking");
    assert_eq!(
        expanded_rows[header..header + 9]
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        [
            "    ⠋ Thinking: Settling",
            "      Reading",
            "      Read the plan.",
            "",
            "      Weighing",
            "      Weighed it.",
            "",
            "      Settling",
            "      Settling on it.",
        ],
        "a reader who asks watches the prose stream under the live row, sections and \
         all: {}",
        expanded_rows.join("\n")
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "Thinking") as u16,
    )
    .expect("hide the streaming prose again");

    assert_eq!(
        rendered_application_rows_at(&application, 80, 24),
        collapsed,
        "clicking the live row again returns it to the single line it opened from"
    );
}

#[test]
fn a_live_reasoning_group_settles_into_its_thought_row_without_moving() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = live_reasoning_run_snapshot(
        session_id,
        workspace.path(),
        ReasoningBlock::streaming("Settling", "Settling on it."),
    );
    let streaming_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session still thinking after a settled run");

    let before = rendered_application_rows_at(&application, 80, 18);
    let row = rendered_row(&before, "Thinking");
    assert_eq!(
        before[row].trim_end(),
        "    ⠋ Thinking: Settling",
        "the run is live in place while its latest member streams: {}",
        before.join("\n")
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: next_revision,
                changes: vec![SessionChange::ReasoningStatusChanged {
                    activity_id: streaming_id,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(600),
                }],
            },
        )))
        .expect("project the Reasoning block settling");

    let after = rendered_application_rows_at(&application, 80, 18);
    assert_eq!(
        after[row].trim_end(),
        "    ✓ Thought: Settling · 3 steps · 1s",
        "the same row flips to its settled form where it stood, counting every member \
         and summing what they each spent: {}",
        after.join("\n")
    );
    assert!(
        !after.join("\n").contains("Thinking"),
        "the settled run left no live row behind: {}",
        after.join("\n")
    );
}

#[test]
fn an_interrupted_streaming_block_leaves_the_group_it_was_living_in() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = live_reasoning_run_snapshot(
        session_id,
        workspace.path(),
        ReasoningBlock::streaming("Settling", "Settling on it."),
    );
    let streaming_id = snapshot.activities[2].id();
    let next_revision = SessionRevision(snapshot.revision.0 + 1);
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session still thinking after a settled run");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: next_revision,
                changes: vec![SessionChange::ReasoningStatusChanged {
                    activity_id: streaming_id,
                    status: ActivityStatus::Failed,
                    duration_ms: None,
                }],
            },
        )))
        .expect("project the Reasoning block being cut off");

    let rows = rendered_application_rows_at(&application, 80, 18);
    let rendered = rows.join("\n");

    assert_eq!(
        rows[rendered_row(&rows, "Thought")].trim_end(),
        "    ✓ Thought: Weighing · 2 steps · 700ms",
        "a Group row only ever summarizes thinking that completed, so the run closes \
         over the members that did: {rendered}"
    );
    assert_eq!(
        rows[rendered_row(&rows, "Thinking interrupted")].trim_end(),
        "    × Thinking interrupted: Settling · +1 lines",
        "and the block that was cut off stands outside it as its own row: {rendered}"
    );
    assert!(
        rendered_row(&rows, "Thought") < rendered_row(&rows, "Thinking interrupted"),
        "presentation order is preserved: {rendered}"
    );
}

#[test]
fn interrupting_a_turn_keeps_the_live_reasoning_row_the_reader_had_opened() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let snapshot = live_reasoning_run_snapshot(
        session_id,
        workspace.path(),
        ReasoningBlock::streaming("Settling", "Settling on it."),
    );
    let streaming_id = snapshot.activities[2].id();
    let interrupted_turn = snapshot.turns[0].id;
    let revision = snapshot.revision;
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session still thinking after a settled run");

    let collapsed = rendered_application_rows_at(&application, 80, 40);
    left_click_at(
        &mut application,
        rendered_row(&collapsed, "Thinking") as u16,
    )
    .expect("open the live Reasoning row to watch the prose stream");
    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![
                    SessionChange::ReasoningStatusChanged {
                        activity_id: streaming_id,
                        status: ActivityStatus::Failed,
                        duration_ms: None,
                    },
                    SessionChange::TurnStatusChanged {
                        turn_id: interrupted_turn,
                        status: TurnStatus::Interrupted,
                        settled_at: None,
                    },
                ],
            },
        )))
        .expect("settle the cut-off block and the Turn it belonged to");

    let rendered = rendered_application_rows_at(&application, 80, 40).join("\n");

    for kept in ["Read the plan.", "Weighed it."] {
        assert!(
            rendered.contains(kept),
            "the reader opened the live row and the interrupt must not close over what \
             they were reading, but {kept:?} went with it: {rendered}"
        );
    }
    assert!(
        rendered.contains("× Thinking interrupted: Settling")
            && rendered.contains("Settling on it."),
        "and the block the interrupt cut short stands outside the Group with the prose \
         it got as far as: {rendered}"
    );
}

#[test]
fn a_turn_fold_reveals_its_reasoning_group_in_the_state_the_reader_left_it() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::UserMessage("Explain the Transcript"),
            REASONING_RUN[0],
            REASONING_RUN[1],
            REASONING_RUN[2],
            RunEntry::AgentMessage("It is the history of a Session."),
        ],
    );
    snapshot.session.status = SessionStatus::Idle;
    snapshot.turns[0].status = TurnStatus::Completed;
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose settled Turn only ever thought");

    let folded = rendered_application_rows_at(&application, 80, 24);
    assert!(
        !folded.join("\n").contains("Thought"),
        "the Turn Fold hides the Reasoning Group it stands for: {}",
        folded.join("\n")
    );

    let toggle_turn = |application: &mut Application| {
        let rows = rendered_application_rows_at(application, 80, 24);
        left_click_at(application, rendered_row(&rows, "Worked") as u16)
            .expect("toggle the Turn Fold");
    };

    toggle_turn(&mut application);
    let opened = rendered_application_rows_at(&application, 80, 24);
    assert!(
        opened.join("\n").contains("3 steps")
            && !opened.join("\n").contains("Read the projection."),
        "expanding the Turn reveals the Group collapsed, so opening a Turn spills no \
         thinking prose: {}",
        opened.join("\n")
    );

    left_click_at(&mut application, rendered_row(&opened, "Thought") as u16)
        .expect("expand the Reasoning Group inside the opened Turn");
    let group_expanded = rendered_application_rows_at(&application, 80, 24);
    assert!(
        group_expanded.join("\n").contains("Read the projection."),
        "the Group opens onto its members' prose: {}",
        group_expanded.join("\n")
    );

    toggle_turn(&mut application);
    toggle_turn(&mut application);
    assert_eq!(
        rendered_application_rows_at(&application, 80, 24),
        group_expanded,
        "expanding a Turn again reveals its Group in whatever state the reader left it"
    );
}

#[test]
fn a_settled_turn_renders_as_one_marker_between_its_prompt_and_its_answer() {
    let workspace = workspace_dir();
    let snapshot = settled_turn_session(
        workspace.path(),
        "Run the workflow",
        "The workflow is green.",
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Turn has settled");

    let rows = rendered_application_rows_at(&application, 80, 24).join("\n");

    assert!(
        rows.contains("✓ Worked"),
        "a settled Turn stands as its marker: {rows}"
    );
    for hidden in ["Ran 2 commands", "command 1", "Thought"] {
        assert!(
            !rows.contains(hidden),
            "the Turn Fold hides the work it stands for, but {hidden:?} rendered: {rows}"
        );
    }
    for kept in ["Run the workflow", "The workflow is green."] {
        assert!(
            rows.contains(kept),
            "the Turn's Prompt and its answer stay outside the fold, but {kept:?} is missing: \
             {rows}"
        );
    }
}

/// A Session whose Turn settled after doing work worth folding away: the
/// fixture every Turn Fold interaction test drives.
fn settled_turn_session(
    workspace: &std::path::Path,
    prompt: &'static str,
    answer: &'static str,
) -> suru::protocol::SessionSnapshot {
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace,
        &[
            RunEntry::UserMessage(prompt),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Reading the workflow",
                "Weighed the options.",
                4_000,
            )),
            RunEntry::AgentMessage(answer),
        ],
    );
    // The reader has stopped watching: the Turn settled and the Session went
    // back to idle.
    snapshot.session.status = SessionStatus::Idle;
    snapshot.turns[0].status = TurnStatus::Completed;
    snapshot
}

#[test]
fn a_settled_turn_fold_marker_renders_duration_without_tokens_or_cost() {
    let workspace = workspace_dir();
    let mut snapshot = settled_turn_session(
        workspace.path(),
        "Measure the workflow",
        "The workflow is measured.",
    );
    snapshot.turns[0].started_at = Some(SessionTimestamp(1_755_000_000_000));
    snapshot.turns[0].settled_at = Some(SessionTimestamp(1_755_000_012_000));
    let mut duration_only = connected_application(workspace.path());
    duration_only
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach a settled Turn without Usage");
    let duration_only_rows = rendered_application_rows_at(&duration_only, 80, 24).join("\n");
    assert!(
        duration_only_rows.contains("✓ Worked for 12s"),
        "a Turn without Usage retains its duration marker: {duration_only_rows}"
    );
    assert!(!duration_only_rows.contains("tokens"));

    snapshot.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(1_200),
        cache_read_tokens: Some(8_000),
        cache_write_tokens: Some(400),
        output_tokens: Some(900),
        reasoning_tokens: Some(2_100),
        native_meter: NativeMeter::from_units(3.25),
        model_context_window: Some(200_000),
    });
    snapshot.turns[0].cost = Cost::from_usd(0.03);
    snapshot.turns[0].cost_basis = Some(CostBasis::Reported);
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a measured settled Turn");

    let rows = rendered_application_rows_at(&application, 80, 24).join("\n");

    assert!(
        rows.contains("✓ Worked for 12s"),
        "the marker retains the Turn duration when usage is known: {rows}"
    );
    assert!(
        !rows.contains("tokens"),
        "token counts stay out of the fold marker: {rows}"
    );
    let marker = rows
        .lines()
        .find(|line| line.contains("✓ Worked"))
        .expect("settled Turn fold marker");
    assert_eq!(marker.trim(), "✓ Worked for 12s");
    assert!(
        !rows.contains("3.25"),
        "the Provider-native premium-request meter has no Turn Fold surface: {rows}"
    );
}

/// Appends a second settled Turn to `snapshot`, so a test can watch what one
/// Turn Fold does to the Turns around it.
fn append_settled_turn(
    snapshot: &mut suru::protocol::SessionSnapshot,
    prompt: &str,
    reasoning: &str,
    answer: &str,
) {
    let turn_id = TurnId::new();
    snapshot.turns.push(Turn {
        id: turn_id,
        prompt_id: Some(PromptId::new()),
        agent: None,
        status: TurnStatus::Completed,
        started_at: None,
        settled_at: None,
        last_output_at: None,
        usage: None,
        cost: None,
        cost_basis: None,
        cost_details: None,
    });
    for (role, content) in [(MessageRole::User, prompt), (MessageRole::Agent, answer)] {
        let message = Message {
            id: MessageId::new(),
            turn_id,
            role,
            status: MessageStatus::Completed,
            content: content.to_owned(),
            skill_invocations: Vec::new(),
            truncated: false,
        };
        if role == MessageRole::Agent {
            let activity = Activity::Reasoning {
                id: ActivityId::new(),
                turn_id,
                status: ActivityStatus::Completed,
                title: Some(reasoning.to_owned()),
                content: "Weighed the options.".to_owned(),
                content_truncated: false,
                duration_ms: None,
            };
            snapshot.transcript.push(TranscriptItem::Activity {
                activity_id: activity.id(),
            });
            snapshot.activities.push(activity);
        }
        snapshot.transcript.push(TranscriptItem::Message {
            message_id: message.id,
        });
        snapshot.messages.push(message);
    }
}

#[test]
fn a_settled_turn_whose_only_work_was_empty_reasoning_shows_no_marker() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::UserMessage("Explain the Transcript"),
            RunEntry::Reasoning(ReasoningBlock::EMPTY),
            RunEntry::AgentMessage("It is the history of a Session."),
        ],
    );
    snapshot.session.status = SessionStatus::Idle;
    snapshot.turns[0].status = TurnStatus::Completed;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose settled Turn only ever thought emptily");

    let rows = rendered_application_rows_at(&application, 80, 24).join("\n");

    assert!(
        !rows.contains("Worked"),
        "a Turn Fold covering only invisible work has nothing to disclose, so no marker \
         stands for it: {rows}"
    );
    for kept in ["Explain the Transcript", "It is the history of a Session."] {
        assert!(
            rows.contains(kept),
            "the Turn's Prompt and its answer still render, but {kept:?} is missing: {rows}"
        );
    }
}

#[test]
fn a_settled_turn_whose_only_work_was_an_empty_file_change_shows_no_marker() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::UserMessage("Change the file"),
            RunEntry::FileChange,
            RunEntry::AgentMessage("There was nothing to change."),
        ],
    );
    snapshot.session.status = SessionStatus::Idle;
    snapshot.turns[0].status = TurnStatus::Completed;
    let Activity::FileChange { changes, .. } = &mut snapshot.activities[0] else {
        panic!("the fixture carries a File Change");
    };
    changes.clear();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose settled Turn changed no files");

    let rows = rendered_application_rows_at(&application, 80, 24).join("\n");

    assert!(
        !rows.contains("Worked"),
        "a Turn Fold covering an empty File Change has nothing to disclose, so no marker \
         stands for it: {rows}"
    );
    for kept in ["Change the file", "There was nothing to change."] {
        assert!(
            rows.contains(kept),
            "the Turn's Prompt and answer still render, but {kept:?} is missing: {rows}"
        );
    }
}

#[test]
fn a_settled_turn_loses_its_marker_only_when_hidden_reasoning_was_all_it_did() {
    let workspace = workspace_dir();
    let settled_turn_of = |entries: &[RunEntry]| {
        let mut snapshot = command_run_snapshot(SessionId::new(), workspace.path(), entries);
        snapshot.session.status = SessionStatus::Idle;
        snapshot.turns[0].status = TurnStatus::Completed;
        session_opened_under(
            workspace.path(),
            settings_with_reasoning(ReasoningVisibility::Hidden),
            &["transcript.reasoningVisibility"],
            snapshot,
        )
    };

    let only_thinking = settled_turn_of(&[
        RunEntry::UserMessage("Explain the Transcript"),
        RunEntry::Reasoning(ReasoningBlock::thought(
            "Reading the projection",
            "It walks the Transcript twice.",
            72_000,
        )),
        RunEntry::AgentMessage("It is the history of a Session."),
    ]);
    let rows = rendered_application_rows_at(&only_thinking, 80, 24).join("\n");
    assert!(
        !rows.contains("Worked"),
        "a Turn Fold covering only work the reader hid has nothing left to disclose, \
         so no marker stands for it: {rows}"
    );
    for kept in ["Explain the Transcript", "It is the history of a Session."] {
        assert!(
            rows.contains(kept),
            "the Turn's Prompt and its answer still render, but {kept:?} is missing: {rows}"
        );
    }

    let thinking_and_working = settled_turn_of(&[
        RunEntry::UserMessage("Run the workflow"),
        RunEntry::Reasoning(ReasoningBlock::thought(
            "Reading the projection",
            "It walks the Transcript twice.",
            72_000,
        )),
        SUCCESSFUL_COMMAND,
        RunEntry::AgentMessage("The workflow is green."),
    ]);
    assert!(
        rendered_application_rows_at(&thinking_and_working, 80, 24)
            .join("\n")
            .contains("Worked"),
        "a Turn that did anything else still marks how it settled and how long it took"
    );
}

#[test]
fn clicking_a_turn_fold_marker_opens_the_turn_and_folds_it_back() {
    let workspace = workspace_dir();
    let snapshot = settled_turn_session(
        workspace.path(),
        "Run the workflow",
        "The workflow is green.",
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Turn has settled");
    let folded_rows = rendered_application_rows_at(&application, 80, 24);

    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ Worked") as u16,
    )
    .expect("click the Turn Fold marker");

    let expanded_rows = rendered_application_rows_at(&application, 80, 24);
    let expanded = expanded_rows.join("\n");
    for revealed in ["Ran 2 commands", "Reading the workflow"] {
        assert!(
            expanded.contains(revealed),
            "clicking the marker reveals the work the Turn Fold hid, but {revealed:?} is \
             missing: {expanded}"
        );
    }
    assert!(
        expanded.contains("✓ Worked"),
        "the marker stays as the row the Turn folds back from: {expanded}"
    );
    assert_eq!(
        rendered_row(&expanded_rows, "✓ Worked"),
        rendered_row(&folded_rows, "✓ Worked"),
        "the Turn's entries open below the marker, so the row the reader clicked stays put"
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "The workflow is green.") as u16,
    )
    .expect("click the Transcript's output body");

    assert_eq!(
        rendered_application_rows_at(&application, 80, 24),
        expanded_rows,
        "clicking Transcript body text changes nothing"
    );

    left_click_at(
        &mut application,
        rendered_row(&expanded_rows, "✓ Worked") as u16,
    )
    .expect("click the Turn Fold marker again");

    let refolded = rendered_application_rows_at(&application, 80, 24).join("\n");
    for hidden in ["Ran 2 commands", "Reading the workflow"] {
        assert!(
            !refolded.contains(hidden),
            "clicking the marker again folds the Turn back, but {hidden:?} rendered: {refolded}"
        );
    }
    assert!(
        refolded.contains("✓ Worked") && refolded.contains("The workflow is green."),
        "the folded Turn is its marker and its answer again: {refolded}"
    );
}

#[test]
fn toggling_the_turn_posture_flips_every_turn_fold_and_clears_per_turn_overrides() {
    let workspace = workspace_dir();
    let mut snapshot = settled_turn_session(
        workspace.path(),
        "Run the workflow",
        "The workflow is green.",
    );
    append_settled_turn(
        &mut snapshot,
        "Now ship it",
        "Checking the release notes",
        "Shipped.",
    );
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session with two settled Turns");
    let folded_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ Worked") as u16,
    )
    .expect("expand the first Turn by hand");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::TranscriptTurnsToggle,
        )))
        .expect("open every Turn Fold");

    let expanded = rendered_application_rows_at(&application, 80, 36).join("\n");
    for revealed in [
        "Ran 2 commands",
        "Reading the workflow",
        "Checking the release notes",
    ] {
        assert!(
            expanded.contains(revealed),
            "the expanded posture opens every Turn Fold, but {revealed:?} is missing: {expanded}"
        );
    }

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::TranscriptTurnsToggle,
        )))
        .expect("fold every Turn Fold");

    let refolded = rendered_application_rows_at(&application, 80, 36).join("\n");
    for hidden in [
        "Ran 2 commands",
        "Reading the workflow",
        "Checking the release notes",
    ] {
        assert!(
            !refolded.contains(hidden),
            "flipping back folds even the Turn the reader expanded by hand, but {hidden:?} \
             rendered: {refolded}"
        );
    }
    assert_eq!(
        refolded.matches("✓ Worked").count(),
        2,
        "both Turns stand as their markers again: {refolded}"
    );
}

/// The Session update a newer Turn beginning delivers: the Prompt that opened
/// it, the Turn it opened, and the user Message that Turn starts from — the
/// run a client actually reads a Turn starting from.
fn newer_turn_begins(
    session_id: SessionId,
    revision: SessionRevision,
    prompt: &str,
) -> ApplicationEvent {
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    ApplicationEvent::Session(SessionEvent::Updated(SessionUpdate {
        session_id,
        revision,
        changes: vec![
            SessionChange::PromptAdded {
                prompt: Prompt {
                    id: prompt_id,
                    text: prompt.to_owned(),
                    delivery: PromptDelivery::Queue,
                    admission_order: PromptOrder(2),
                    status: PromptStatus::Delivered,
                    skill_invocations: Vec::new(),
                },
            },
            SessionChange::TurnAdded {
                turn: Turn {
                    id: turn_id,
                    prompt_id: Some(prompt_id),
                    agent: None,
                    status: TurnStatus::Active,
                    started_at: None,
                    settled_at: None,
                    last_output_at: None,
                    usage: None,
                    cost: None,
                    cost_basis: None,
                    cost_details: None,
                },
            },
            SessionChange::MessageAdded {
                message: Message {
                    id: MessageId::new(),
                    turn_id,
                    role: MessageRole::User,
                    status: MessageStatus::Completed,
                    content: prompt.to_owned(),
                    truncated: false,
                    skill_invocations: Vec::new(),
                },
            },
        ],
    }))
}

#[test]
fn interrupting_a_turn_holds_its_fold_open_until_a_newer_turn_begins() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Reading the workflow",
                "Weighed the options.",
                4_000,
            )),
            RunEntry::AgentMessage("Two suites in, still going."),
        ],
    );
    let session_id = snapshot.session.id;
    let interrupted_turn = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, interrupted_turn);
    let revision = snapshot.revision;
    let mut application = client_showing_reasoning(workspace.path());
    // A second client watching the same Session, so the test reads what the
    // interrupt does to the reader who asked for it and what it does to a view
    // that did not.
    let mut observer = client_showing_reasoning(workspace.path());
    for client in [&mut application, &mut observer] {
        client
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .expect("attach a Session whose Turn the reader is watching");
    }

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
    let settle = || {
        ApplicationEvent::Session(SessionEvent::Updated(SessionUpdate {
            session_id,
            revision: SessionRevision(revision.0 + 1),
            changes: vec![SessionChange::TurnStatusChanged {
                turn_id: interrupted_turn,
                status: TurnStatus::Interrupted,
                settled_at: None,
            }],
        }))
    };
    for client in [&mut application, &mut observer] {
        client
            .handle_event(settle())
            .expect("settle the interrupted Turn for both clients");
    }

    let observed = rendered_application_rows_at(&observer, 80, 40).join("\n");
    assert!(
        observed.contains("× Stopped") && !observed.contains("Reading the workflow"),
        "the auto-expand is view state of the client that interrupted, so a fresh view of the \
         Session reads the Turn as its marker: {observed}"
    );
    let interrupted = rendered_application_rows_at(&application, 80, 40).join("\n");
    assert!(
        interrupted.contains("× Stopped"),
        "the interrupted Turn marks how it settled: {interrupted}"
    );
    for kept in ["Ran 2 commands", "Reading the workflow"] {
        assert!(
            interrupted.contains(kept),
            "interrupting the Turn holds its fold open so the reader keeps their place, but \
             {kept:?} is missing: {interrupted}"
        );
    }

    application
        .handle_event(newer_turn_begins(
            session_id,
            SessionRevision(revision.0 + 2),
            "Now ship it",
        ))
        .expect("begin a newer Turn");

    let moved_on = rendered_application_rows_at(&application, 80, 40).join("\n");
    for hidden in ["Ran 2 commands", "Reading the workflow"] {
        assert!(
            !moved_on.contains(hidden),
            "a newer Turn re-folds the Turn the interrupt opened, but {hidden:?} rendered: \
             {moved_on}"
        );
    }
    assert!(
        moved_on.contains("× Stopped") && moved_on.contains("Run the workflow"),
        "the re-folded Turn is its marker and its Prompt again: {moved_on}"
    );
}

#[test]
fn a_queued_prompt_starting_in_the_settle_commit_refolds_the_interrupted_turn_at_once() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::UserMessage("Run the workflow"),
            SUCCESSFUL_COMMAND,
            SUCCESSFUL_COMMAND,
            RunEntry::Reasoning(ReasoningBlock::thought(
                "Reading the workflow",
                "Weighed the options.",
                4_000,
            )),
            RunEntry::AgentMessage("Two suites in, still going."),
        ],
    );
    let session_id = snapshot.session.id;
    let interrupted_turn = snapshot.turns[0].id;
    set_turn_in_flight(&mut snapshot, interrupted_turn);
    let revision = snapshot.revision;
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Turn the reader is watching");

    for _ in 0..2 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("request and confirm the interrupt");
    }
    // The server settles an interrupted Turn and starts the queued Prompt's
    // Turn in one commit, so the reader's next Turn begins in the very update
    // that settles the one they stopped.
    let ApplicationEvent::Session(SessionEvent::Updated(next_turn)) = newer_turn_begins(
        session_id,
        SessionRevision(revision.0 + 1),
        "Ship it anyway",
    ) else {
        unreachable!("a newer Turn arrives as a Session update");
    };
    let mut changes = vec![SessionChange::TurnStatusChanged {
        turn_id: interrupted_turn,
        status: TurnStatus::Interrupted,
        settled_at: None,
    }];
    changes.extend(next_turn.changes);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                changes,
                ..next_turn
            },
        )))
        .expect("settle the interrupted Turn and start the queued Prompt's Turn together");

    let rendered = rendered_application_rows_at(&application, 80, 40).join("\n");
    assert!(
        rendered.contains("× Stopped") && rendered.contains("Ship it anyway"),
        "the stopped Turn stands as its marker above the Turn that displaced it: {rendered}"
    );
    for hidden in ["Ran 2 commands", "Reading the workflow"] {
        assert!(
            !rendered.contains(hidden),
            "the reader's own queued Prompt is the newer Turn that re-folds the one they \
             stopped, so the interrupt's expansion never outlives the Settle, but {hidden:?} \
             rendered: {rendered}"
        );
    }
}

#[test]
fn a_newer_turn_refolds_the_turn_the_reader_expanded_by_hand() {
    let workspace = workspace_dir();
    let snapshot = settled_turn_session(
        workspace.path(),
        "Run the workflow",
        "The workflow is green.",
    );
    let session_id = snapshot.session.id;
    let revision = snapshot.revision;
    let mut application = client_showing_reasoning(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach a Session whose Turn has settled");
    let folded_rows = rendered_application_rows_at(&application, 80, 36);
    left_click_at(
        &mut application,
        rendered_row(&folded_rows, "✓ Worked") as u16,
    )
    .expect("expand the settled Turn by hand");
    assert!(
        rendered_application_rows_at(&application, 80, 36)
            .join("\n")
            .contains("Reading the workflow")
    );

    application
        .handle_event(newer_turn_begins(
            session_id,
            SessionRevision(revision.0 + 1),
            "Now ship it",
        ))
        .expect("begin a newer Turn");

    let moved_on = rendered_application_rows_at(&application, 80, 36).join("\n");
    assert!(
        !moved_on.contains("Reading the workflow"),
        "a Turn Fold's expansion lasts only until the reader moves on, unlike a per-entry \
         Fold's sticky override: {moved_on}"
    );
    assert!(
        moved_on.contains("✓ Worked"),
        "the re-folded Turn stands as its marker again: {moved_on}"
    );
}

#[test]
fn the_transcript_keeps_a_row_of_air_above_the_composer() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let (session_id, _) = crate::support::enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            navigable_session_snapshot(session_id, workspace.path(), 8),
        )))
        .expect("attach a Transcript longer than the viewport");

    let rows = rendered_application_rows_at(&application, 80, 15);

    let composer_top = rendered_row(&rows, "┌");
    let gap = composer_top
        .checked_sub(1)
        .expect("the composer is not the first row");
    assert!(
        rows[gap].trim().is_empty(),
        "the Transcript keeps a row of air above the composer, but row {gap} reads {:?}\nframe:\n{}",
        rows[gap],
        rows.join("\n")
    );
}

#[test]
fn the_transcript_keeps_its_margin_when_a_pending_panel_docks_below_it() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let (session_id, _) = crate::support::enter_session(&mut application, workspace.path());
    let mut snapshot = navigable_session_snapshot(session_id, workspace.path(), 8);
    snapshot.session.status = SessionStatus::Active;
    snapshot.prompts.push(Prompt {
        id: PromptId::new(),
        text: "Run this later".to_owned(),
        delivery: PromptDelivery::Queue,
        admission_order: PromptOrder(99),
        status: PromptStatus::Pending,
        skill_invocations: Vec::new(),
    });
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("attach a long Transcript with a queued Prompt");

    let rows = rendered_application_rows_at(&application, 80, 20);

    let pending_top = rendered_row(&rows, "Pending ·");
    let margin = pending_top
        .checked_sub(1)
        .expect("the pending panel is not the first row");
    assert!(
        rows[margin].trim().is_empty(),
        "the margin belongs to the Transcript, so it stays under the last entry whatever docks \
         below it, but row {margin} reads {:?}\nframe:\n{}",
        rows[margin],
        rows.join("\n")
    );
}

#[test]
fn transcript_selection_unwraps_text_and_persists_after_copy() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    pin_release_copy(&mut application);
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace.path(), 1);
    snapshot
        .messages
        .iter_mut()
        .find(|m| m.role == MessageRole::Agent)
        .unwrap()
        .content = "Alpha bravo charlie delta echo foxtrot golf hotel   ".into();
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 32, 24);
    let start = text_position(&buffer, "Alpha");
    let end = text_position(&buffer, "hotel");
    let mouse = |kind, (x, y)| {
        InputEvent::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    };
    application
        .handle_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), start))
        .unwrap();
    application
        .handle_terminal_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            (end.0 + 10, end.1),
        ))
        .unwrap();
    let selected = rendered_application_buffer(&application, 32, 24);
    assert!(selected[start].modifier.contains(Modifier::REVERSED));
    let transition = application
        .handle_terminal_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            (end.0 + 10, end.1),
        ))
        .unwrap();
    assert!(
        matches!(transition, ApplicationTransition::CopyToClipboard(ref text)
        if text.text == "Alpha bravo charlie delta echo foxtrot golf hotel"),
        "{transition:?}"
    );
    assert!(
        rendered_application_buffer(&application, 32, 24)[start]
            .modifier
            .contains(Modifier::REVERSED)
    );
}

fn selection_mouse(kind: MouseEventKind, position: (u16, u16)) -> InputEvent {
    InputEvent::Mouse(MouseEvent {
        kind,
        column: position.0,
        row: position.1,
        modifiers: KeyModifiers::NONE,
    })
}

fn select_transcript_payload(
    application: &mut Application,
    start: (u16, u16),
    end: (u16, u16),
) -> suru::tui::ClipboardContent {
    use crossterm::event::MouseButton;
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(MouseButton::Left),
            start,
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            end,
        ))
        .unwrap();
    match application
        .handle_terminal_event(selection_mouse(MouseEventKind::Up(MouseButton::Left), end))
        .unwrap()
    {
        ApplicationTransition::CopyToClipboard(text) => text,
        other => panic!("expected copy, got {other:?}"),
    }
}

fn select_transcript(application: &mut Application, start: (u16, u16), end: (u16, u16)) -> String {
    select_transcript_payload(application, start, end).text
}

#[test]
fn markdown_selection_balances_partial_inline_formatting_without_unselected_words() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "Before **bold *chosen words* after** outside",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "chosen words");
    assert_eq!(
        select_transcript_payload(&mut application, start, (start.0 + 5, start.1)),
        suru::tui::ClipboardContent {
            text: "***chosen***".into(),
            html: Some("<p><em><strong>chosen</strong></em></p>\n".into()),
        }
    );
}

#[test]
fn transcript_selection_copies_source_lines_skipping_chrome_and_keeping_separators() {
    let workspace = workspace_dir();
    for (entries, first, last, expected) in [
        (
            vec![RunEntry::AgentMessage(
                "```rust\nlet answer = 42;   \n\nprintln!(\"界🙂\");\n```",
            )],
            "rust",
            "println!",
            "let answer = 42;\n\nprintln!(\"界🙂\");",
        ),
        (
            vec![RunEntry::Command(ActivityStatus::Completed, Some(0))],
            "command 1",
            "command 1",
            "command 1",
        ),
        (
            vec![
                RunEntry::AgentMessage("First source line"),
                RunEntry::UserMessage("Second source line"),
            ],
            "First source",
            "Second source",
            "First source line\n\nSecond source line",
        ),
    ] {
        let snapshot = command_run_snapshot(SessionId::new(), workspace.path(), &entries);
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 60, 24);
        let (x, y) = text_position(&buffer, first);
        let (_, end_y) = text_position(&buffer, last);
        let copied = select_transcript(&mut application, (x.saturating_sub(2), y), (58, end_y));
        assert_eq!(copied, expected);
    }
}

#[test]
fn transcript_selection_survives_typing_and_clears_on_press_escape_resize_and_session_switch() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("Selected words")],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .unwrap();
    for action in 0..4 {
        let buffer = rendered_application_buffer(&application, 60, 24);
        let start = text_position(&buffer, "Selected words");
        assert_eq!(
            select_transcript(&mut application, start, (start.0 + 13, start.1)),
            "Selected words"
        );
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert!(
            rendered_application_buffer(&application, 60, 24)[start]
                .modifier
                .contains(Modifier::REVERSED)
        );
        let transition = match action {
            0 => application.handle_terminal_event(selection_mouse(
                MouseEventKind::Down(MouseButton::Left),
                (0, 0),
            )),
            1 => application.handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))),
            2 => application.handle_terminal_event(InputEvent::Resize(61, 24)),
            _ => application.handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage("Selected words")],
            ))),
        }
        .unwrap();
        assert!(
            action == 3 || matches!(transition, ApplicationTransition::Continue),
            "{transition:?}"
        );
        assert!(
            !rendered_application_buffer(&application, 60, 24)[start]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }
}

#[test]
fn transcript_selection_clears_when_streaming_reprojects_but_survives_a_pure_append() {
    let workspace = workspace_dir();
    for streaming in [false, true] {
        let mut snapshot = command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage("Selected words")],
        );
        snapshot.messages[0].status = MessageStatus::Streaming;
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 60, 24);
        let start = text_position(&buffer, "Selected words");
        select_transcript(&mut application, start, (start.0 + 13, start.1));
        let change = if streaming {
            SessionChange::MessageContentAppended {
                message_id: snapshot.messages[0].id,
                content: " more words".into(),
            }
        } else {
            let mut message = snapshot.messages[0].clone();
            message.id = MessageId::new();
            message.content = "New message appended".into();
            SessionChange::MessageAdded { message }
        };
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                SessionUpdate {
                    session_id: snapshot.session.id,
                    revision: SessionRevision(snapshot.revision.0 + 1),
                    changes: vec![change],
                },
            )))
            .unwrap();
        let copy = application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::TextSelectionCopy,
            )))
            .unwrap();
        assert_eq!(
            matches!(copy, ApplicationTransition::CopyToClipboard(_)),
            !streaming
        );
        let after = rendered_application_buffer(&application, 60, 24);
        let position = text_position(&after, "Selected words");
        assert_eq!(
            after[position].modifier.contains(Modifier::REVERSED),
            !streaming
        );
    }
}

#[test]
fn transcript_selection_stays_on_text_when_the_wheel_scrolls() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let text = (0..35)
        .map(|i| format!("unique row {i:02}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("placeholder")],
    );
    snapshot.messages[0].content = text;
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&buffer, "unique row 31");
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(MouseButton::Left),
            start,
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            (start.0 + 12, start.1),
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(MouseEventKind::ScrollUp, start))
        .unwrap();
    let after = rendered_application_buffer(&application, 60, 24);
    let moved = text_position(&after, "unique row 31");
    assert_ne!(start.1, moved.1);
    assert!(after[moved].modifier.contains(Modifier::REVERSED));
    let copy = application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(MouseButton::Left),
            (moved.0 + 12, moved.1),
        ))
        .unwrap();
    assert!(
        matches!(copy, ApplicationTransition::CopyToClipboard(ref text) if text.text == "unique row 31"),
        "{copy:?}"
    );
}

#[test]
fn transcript_selection_joins_an_oversize_split_line() {
    let workspace = workspace_dir();
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("placeholder")],
    );
    let source = format!("START {} END", "x".repeat(36_000));
    snapshot.messages[0].content = source.clone();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 32, 1600);
    let start = text_position(&buffer, "START");
    let end = text_position(&buffer, "END");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 2, end.1)),
        source
    );
}

#[test]
fn transcript_selection_clamps_to_the_centered_column_and_never_highlights_the_composer_or_sidebar()
{
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("First line\n\nLast line")],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 160, 30);
    let start = text_position(&buffer, "First line");
    let end = text_position(&buffer, "Last line");
    // The pointer leaves both the column and the Transcript's vertical extent.
    assert_eq!(
        select_transcript(&mut application, start, (159, 29)),
        "First line\n\nLast line"
    );
    let selected = rendered_application_buffer(&application, 160, 30);
    for y in 0..30 {
        for x in 0..160 {
            if selected[(x, y)].modifier.contains(Modifier::REVERSED)
                && !buffer[(x, y)].modifier.contains(Modifier::REVERSED)
            {
                assert!(y >= start.1 && y <= end.1);
                assert!(x >= start.0 - 2 && x < start.0 - 2 + 80, "{x},{y}");
            }
        }
    }
    // The highlight covers the text a copy would hold and nothing else: the
    // blank row between the lines, the gutter to the left of "Last line", and
    // the empty cells to the right of "First line" all stay as drawn.
    let reversed = |position: (u16, u16)| selected[position].modifier.contains(Modifier::REVERSED);
    for x in 0..160 {
        assert!(!reversed((x, start.1 + 1)), "blank row lit at {x}");
    }
    for x in 0..end.0 {
        assert!(!reversed((x, end.1)), "gutter lit at {x}");
    }
    assert!(!reversed((start.0 + "First line".len() as u16, start.1)));
    assert!(reversed((start.0, start.1)));
    assert!(reversed((start.0 + "First line".len() as u16 - 1, start.1)));
    assert!(reversed((end.0, end.1)));
    assert!(reversed((end.0 + "Last line".len() as u16 - 1, end.1)));
}

#[test]
fn transcript_selection_copies_a_wide_character_once_in_either_direction_and_blank_rows_as_empty_lines()
 {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[
            RunEntry::AgentMessage("A界🙂Z"),
            RunEntry::UserMessage("Last"),
        ],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&buffer, "A界");
    let last = text_position(&buffer, "Last");
    assert_eq!(
        select_transcript(
            &mut application,
            (start.0 + 1, start.1),
            (start.0 + 2, start.1)
        ),
        "界"
    );
    assert_eq!(
        select_transcript(
            &mut application,
            (start.0 + 4, start.1),
            (start.0 + 1, start.1)
        ),
        "界🙂"
    );
    assert_eq!(
        select_transcript(&mut application, start, (58, last.1 - 1)),
        "A界🙂Z\n"
    );
}

#[test]
fn transcript_selection_clears_when_a_fold_is_toggled() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&buffer, "cargo test");
    select_transcript(&mut application, start, (start.0 + 9, start.1));
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::TranscriptFoldsToggle,
        )))
        .unwrap();
    let after = rendered_application_buffer(&application, 60, 24);
    assert!(!after[start].modifier.contains(Modifier::REVERSED));
}

#[test]
fn transcript_selection_past_a_wrapped_rows_text_does_not_copy_the_next_rows_character() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("abcdefghijklmnopqrstuvw界tail")],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 28, 24);
    let start = text_position(&buffer, "abcdefghijklmnopqrstuvw");
    assert_eq!(
        select_transcript(&mut application, start, (27, start.1)),
        "abcdefghijklmnopqrstuvw"
    );
}

#[test]
fn transcript_selection_drag_returning_to_its_anchor_does_not_select_one_cell_or_click() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::Command(ActivityStatus::Completed, Some(0))],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let before = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&before, "command 1");
    for (kind, position) in [
        (MouseEventKind::Down(MouseButton::Left), start),
        (
            MouseEventKind::Drag(MouseButton::Left),
            (start.0 + 2, start.1),
        ),
        (MouseEventKind::Drag(MouseButton::Left), start),
        (MouseEventKind::Up(MouseButton::Left), start),
    ] {
        assert_eq!(
            application
                .handle_terminal_event(selection_mouse(kind, position))
                .unwrap(),
            ApplicationTransition::Continue
        );
    }
    assert_eq!(rendered_application_buffer(&application, 60, 24), before);
}

#[test]
fn transcript_selection_highlights_a_wide_glyph_when_dragging_from_its_trailing_cell() {
    let workspace = workspace_dir();
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::AgentMessage("A界🙂Z")],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let before = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&before, "A界");
    assert_eq!(
        select_transcript(
            &mut application,
            (start.0 + 2, start.1),
            (start.0 + 5, start.1)
        ),
        "界🙂Z"
    );
    let after = rendered_application_buffer(&application, 60, 24);
    assert!(
        after[(start.0 + 1, start.1)]
            .modifier
            .contains(Modifier::REVERSED)
    );
    assert!(!after[start].modifier.contains(Modifier::REVERSED));
}

#[test]
fn transcript_selection_unwraps_user_messages_without_losing_whitespace_or_empty_source_lines() {
    let workspace = workspace_dir();
    let source = "First words                        next words that wrap\n\nLast line   ";
    let snapshot = command_run_snapshot(
        SessionId::new(),
        workspace.path(),
        &[RunEntry::UserMessage(source)],
    );
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 28, 24);
    let start = text_position(&buffer, "First words");
    let end = text_position(&buffer, "Last line");
    assert_eq!(
        select_transcript(&mut application, start, (27, end.1)),
        "First words                        next words that wrap\n\nLast line"
    );
}

#[test]
fn transcript_selection_unwraps_the_visible_tail_of_a_folded_command_line() {
    let workspace = workspace_dir();
    let selected = format!("SELECT {} END", "z".repeat(100));
    let output = format!("{}{}", "x".repeat(500), selected);
    let (snapshot, _) =
        command_activity_session(workspace.path(), ActivityStatus::Completed, &output, false);
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    // Open its Peek, which can start partway through a source line.
    let buffer = rendered_application_buffer(&application, 60, 24);
    let header = text_position(&buffer, "cargo test");
    super::support::click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: header.0,
            row: header.1,
            modifiers: KeyModifiers::NONE,
        },
    )
    .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let start = text_position(&buffer, "SELECT");
    let end = text_position(&buffer, "END");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 2, end.1)),
        selected
    );
}

#[test]
fn manual_selection_waits_for_copy_and_explicit_copy_clears_in_both_modes() {
    use crossterm::event::MouseButton;
    use suru::protocol::TextSelectionCopy;
    let workspace = workspace_dir();
    for mode in [TextSelectionCopy::Manual, TextSelectionCopy::Release] {
        for right_click in [false, true] {
            let mut application = connected_application(workspace.path());
            application
                .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                    SessionId::new(),
                    workspace.path(),
                    &[RunEntry::AgentMessage("Selected words")],
                )))
                .unwrap();
            let mut settings = EffectiveSettings::default();
            settings.text_selection.copy = match mode {
                TextSelectionCopy::Manual => TextSelectionCopy::Release,
                TextSelectionCopy::Release => TextSelectionCopy::Manual,
            };
            deliver_settings(&mut application, settings, &["textSelection.copy"]);
            let buffer = rendered_application_buffer(&application, 60, 24);
            let start = text_position(&buffer, "Selected words");
            let end = (start.0 + 13, start.1);
            for (kind, position) in [
                (MouseEventKind::Down(MouseButton::Left), start),
                (MouseEventKind::Drag(MouseButton::Left), end),
            ] {
                application
                    .handle_terminal_event(selection_mouse(kind, position))
                    .unwrap();
            }
            // A settings update during the drag governs this release immediately.
            let mut settings = EffectiveSettings::default();
            settings.text_selection.copy = mode;
            deliver_settings(&mut application, settings, &["textSelection.copy"]);
            let released = application
                .handle_terminal_event(selection_mouse(MouseEventKind::Up(MouseButton::Left), end))
                .unwrap();
            assert_eq!(
                released,
                if mode == TextSelectionCopy::Manual {
                    ApplicationTransition::Continue
                } else {
                    ApplicationTransition::CopyToClipboard(suru::tui::ClipboardContent {
                        text: "Selected words".into(),
                        html: Some("<p>Selected words</p>\n".into()),
                    })
                }
            );
            assert!(
                rendered_application_buffer(&application, 60, 24)[start]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
            let event = if right_click {
                selection_mouse(MouseEventKind::Down(MouseButton::Right), (0, 0))
            } else {
                InputEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            };
            assert_eq!(
                application.handle_terminal_event(event).unwrap(),
                ApplicationTransition::CopyToClipboard(suru::tui::ClipboardContent {
                    text: "Selected words".into(),
                    html: Some("<p>Selected words</p>\n".into())
                })
            );
            assert!(
                !rendered_application_buffer(&application, 60, 24)[start]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
    }
}

fn pin_release_copy(application: &mut Application) {
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = suru::protocol::TextSelectionCopy::Release;
    deliver_settings(application, settings, &["textSelection.copy"]);
}

fn edge_drag_application(workspace: &std::path::Path) -> (Application, String) {
    let mut application = connected_application(workspace);
    pin_release_copy(&mut application);
    let lines = (0..60).map(|n| format!("line {n:02}")).collect::<Vec<_>>();
    let text = lines.join("\n");
    let mut snapshot = command_run_snapshot(
        SessionId::new(),
        workspace,
        &[RunEntry::UserMessage("placeholder")],
    );
    snapshot
        .messages
        .iter_mut()
        .find(|m| m.role == MessageRole::User)
        .unwrap()
        .content = text.clone();
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    (application, text)
}

#[test]
fn transcript_edge_drag_scrolls_on_events_and_ticks_and_copies_the_whole_selection() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let (mut application, text) = edge_drag_application(workspace.path());
    let buffer = rendered_application_buffer(&application, 80, 20);
    let start = text_position(&buffer, "line 59");
    let top = buffer_rows(&buffer)
        .iter()
        .position(|row| row.contains("line "))
        .unwrap() as u16;
    assert!(top > 0);
    let outside = (start.0, top - 1);
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(MouseButton::Left),
            (start.0 + 6, start.1),
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            outside,
        ))
        .unwrap();
    assert!(
        application.wants_spinner(),
        "holding above the edge arms presentation ticks"
    );
    let after_drag = rendered_application_buffer(&application, 80, 20);
    assert_ne!(
        buffer_rows(&buffer),
        buffer_rows(&after_drag),
        "the drag itself scrolls one row"
    );
    for _ in 0..80 {
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .unwrap();
    }
    let selected = rendered_application_buffer(&application, 80, 20);
    let first = text_position(&selected, "line 00");
    assert!(selected[first].modifier.contains(Modifier::REVERSED));
    let transition = application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(MouseButton::Left),
            outside,
        ))
        .unwrap();
    assert!(
        matches!(transition, ApplicationTransition::CopyToClipboard(ref copied) if copied.text == text),
        "{transition:?}"
    );
    assert!(
        !application.wants_spinner(),
        "release disarms idle presentation"
    );
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .unwrap();
    assert_eq!(
        buffer_rows(&selected),
        buffer_rows(&rendered_application_buffer(&application, 80, 20))
    );
}

#[test]
fn transcript_edge_drag_speed_tracks_distance_and_reentry_disarms_ticks() {
    use crossterm::event::MouseButton;
    for distance in [1, 3] {
        let workspace = workspace_dir();
        let (mut application, text) = edge_drag_application(workspace.path());
        rendered_application_buffer(&application, 80, 20);
        for _ in 0..10 {
            application
                .handle_event(ApplicationEvent::Command(CommandId::ScrollTranscriptPageUp))
                .unwrap();
        }
        let buffer = rendered_application_buffer(&application, 80, 20);
        let start = text_position(&buffer, "line 00");
        let rows = buffer_rows(&buffer);
        let bottom = rows
            .iter()
            .rposition(|row| row.contains("┃ line "))
            .unwrap() as u16;
        let last_number = |buffer: &Buffer| -> usize {
            buffer_rows(buffer)
                .iter()
                .filter_map(|row| {
                    row.split_once("line ")
                        .and_then(|(_, tail)| tail.trim().parse().ok())
                })
                .last()
                .unwrap()
        };
        let before = last_number(&buffer);
        let outside = (start.0 + 6, bottom + distance);
        application
            .handle_terminal_event(selection_mouse(
                MouseEventKind::Down(MouseButton::Left),
                start,
            ))
            .unwrap();
        application
            .handle_terminal_event(selection_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                outside,
            ))
            .unwrap();
        let selected = rendered_application_buffer(&application, 80, 20);
        assert_eq!(
            last_number(&selected),
            before + 1,
            "every drag event scrolls exactly one row regardless of distance"
        );
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .unwrap();
        let selected = rendered_application_buffer(&application, 80, 20);
        assert_eq!(
            last_number(&selected),
            before + 1 + usize::from(distance),
            "before {rows:?} after {:?}",
            buffer_rows(&selected)
        );
        let focus = text_position(&selected, &format!("line {:02}", last_number(&selected)));
        assert!(selected[focus].modifier.contains(Modifier::REVERSED));
        application
            .handle_terminal_event(selection_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                (outside.0, bottom),
            ))
            .unwrap();
        assert!(!application.wants_spinner());
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .unwrap();
        assert_eq!(
            buffer_rows(&selected),
            buffer_rows(&rendered_application_buffer(&application, 80, 20))
        );
        application
            .handle_terminal_event(selection_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                outside,
            ))
            .unwrap();
        assert!(application.wants_spinner());
        for _ in 0..80 {
            application
                .handle_event(ApplicationEvent::SpinnerTick)
                .unwrap();
        }
        let at_end = rendered_application_buffer(&application, 80, 20);
        assert_eq!(last_number(&at_end), 59);
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .unwrap();
        assert_eq!(
            buffer_rows(&at_end),
            buffer_rows(&rendered_application_buffer(&application, 80, 20))
        );
        let transition = application
            .handle_terminal_event(selection_mouse(
                MouseEventKind::Up(MouseButton::Left),
                outside,
            ))
            .unwrap();
        assert!(
            matches!(transition, ApplicationTransition::CopyToClipboard(ref copied) if copied.text == text),
            "{transition:?}"
        );
        assert!(!application.wants_spinner());
    }
}

#[test]
fn reconnecting_cancels_a_held_transcript_edge_drag() {
    use crossterm::event::MouseButton;
    let workspace = workspace_dir();
    let (mut application, _) = edge_drag_application(workspace.path());
    let buffer = rendered_application_buffer(&application, 80, 20);
    let start = text_position(&buffer, "line 59");
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(MouseButton::Left),
            start,
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            (start.0, 0),
        ))
        .unwrap();
    assert!(application.wants_spinner());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            suru::managed_client::RecoveryStatus {
                attempt: 1,
                retry_in: Duration::ZERO,
            },
        )))
        .unwrap();
    application
        .handle_event(ApplicationEvent::ReconnectGraceElapsed(
            suru::protocol::Outlook::Local,
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(MouseButton::Left),
            (start.0, 0),
        ))
        .unwrap();
    assert!(
        !application.wants_spinner(),
        "recovery must not retain a held drag behind its overlay"
    );
    let stopped = rendered_application_buffer(&application, 80, 20);
    application
        .handle_event(ApplicationEvent::SpinnerTick)
        .unwrap();
    assert_eq!(
        buffer_rows(&stopped),
        buffer_rows(&rendered_application_buffer(&application, 80, 20))
    );
}

#[test]
fn markdown_selection_keeps_block_structure_and_partial_list_indentation() {
    let workspace = workspace_dir();
    for (source, first, last, last_width, expected, expected_html) in [
        (
            "## Heading\n\nA **bold** and *soft* `value` [link](https://example.test).\n\n> quoted words",
            "Heading",
            "quoted words",
            12,
            "## Heading\n\nA **bold** and *soft* `value` [link](https://example.test).\n\n> quoted words",
            "<h2>Heading</h2>\n<p>A <strong>bold</strong> and <em>soft</em> <code>value</code> <a href=\"https://example.test\">link</a>.</p>\n<blockquote>\n<p>quoted words</p>\n</blockquote>\n",
        ),
        (
            "3. before **chosen words** after\n   - nested *child text*\n4. outside",
            "chosen",
            "child text",
            5,
            "3. **chosen words** after\n   - nested *child*",
            "<ol start=\"3\">\n<li><strong>chosen words</strong> after\n<ul>\n<li>nested <em>child</em></li>\n</ul>\n</li>\n</ol>\n",
        ),
    ] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage(source)],
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 100, 32);
        let start = text_position(&buffer, first);
        let end = text_position(&buffer, last);
        let content =
            select_transcript_payload(&mut application, start, (end.0 + last_width - 1, end.1));
        assert_eq!(content.text, expected);
        assert_eq!(content.html.as_deref(), Some(expected_html));
    }
}

#[test]
fn markdown_selection_ignores_quote_chrome_across_blocks_and_tables() {
    let workspace = workspace_dir();
    let source = "> first\n>\n> second\n>\n> - alpha\n> - beta\n>\n> | Key | Value |\n> | --- | --- |\n> | one | two |";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 32);
    let start = text_position(&buffer, "first");
    let end = text_position(&buffer, "two");
    let content = select_transcript_payload(&mut application, start, (end.0 + 2, end.1));

    assert_eq!(content.text, source);
    let html = content.html.expect("Markdown selection carries rich HTML");
    assert!(html.starts_with("<blockquote>\n<p>first</p>"));
    assert!(html.contains("<li>beta</li>"));
    assert!(html.contains("<table><thead>"));
    assert!(html.ends_with("</table>\n</blockquote>\n"));
}

#[test]
fn markdown_selection_preserves_parenthesized_ordinal_shape_and_numbering() {
    let workspace = workspace_dir();
    let source = "9) nine\n10) ten";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "nine");
    let end = text_position(&buffer, "ten");
    let content = select_transcript_payload(&mut application, start, (end.0 + 2, end.1));

    assert_eq!(content.text, source);
    assert_eq!(
        content.html.as_deref(),
        Some("<ol start=\"9\">\n<li>nine</li>\n<li>ten</li>\n</ol>\n")
    );
}

#[test]
fn markdown_selection_restores_a_heading_nested_in_a_quoted_list() {
    let workspace = workspace_dir();
    let source = "> - ## Title";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "Title");
    let content = select_transcript_payload(&mut application, start, (start.0 + 4, start.1));

    assert_eq!(content.text, source);
    let html = content.html.expect("Markdown selection carries rich HTML");
    assert!(html.starts_with("<blockquote>\n<ul>"));
    assert!(html.contains("<h2>Title</h2>"));
    assert!(html.ends_with("</ul>\n</blockquote>\n"));
}

#[test]
fn markdown_selection_round_trips_task_items_and_strikethrough() {
    let workspace = workspace_dir();
    let source = "- [ ] pending\n  - [x] ~~done~~";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "pending");
    let end = text_position(&buffer, "done");
    let content = select_transcript_payload(&mut application, start, (end.0 + 3, end.1));

    assert_eq!(content.text, source);
    assert_eq!(
        content.html.as_deref(),
        Some(
            "<ul>\n<li><input disabled=\"\" type=\"checkbox\"/>\npending\n<ul>\n<li><input disabled=\"\" type=\"checkbox\" checked=\"\"/>\n<del>done</del></li>\n</ul>\n</li>\n</ul>\n"
        )
    );
}

#[test]
fn markdown_selection_keeps_task_state_from_loose_item_paragraphs() {
    let workspace = workspace_dir();
    let source = "- [x] one\n\n- [ ] two";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "one");
    let end = text_position(&buffer, "two");
    let content = select_transcript_payload(&mut application, start, (end.0 + 2, end.1));

    assert_eq!(content.text, "- [x] one\n- [ ] two");
    assert_eq!(
        content.html.as_deref(),
        Some(
            "<ul>\n<li><input disabled=\"\" type=\"checkbox\" checked=\"\"/>\none</li>\n<li><input disabled=\"\" type=\"checkbox\"/>\ntwo</li>\n</ul>\n"
        )
    );
}

#[test]
fn markdown_selection_preserves_full_and_partial_standalone_strikethrough() {
    let workspace = workspace_dir();
    for (first, last, last_width, expected, expected_html) in [
        (
            "whole",
            "strike",
            6,
            "~~whole strike~~",
            "<p><del>whole strike</del></p>\n",
        ),
        (
            "strike",
            "strike",
            6,
            "~~strike~~",
            "<p><del>strike</del></p>\n",
        ),
    ] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage("~~whole strike~~ plain")],
            )))
            .unwrap();

        let buffer = rendered_application_buffer(&application, 80, 24);
        let start = text_position(&buffer, first);
        let end = text_position(&buffer, last);
        let content =
            select_transcript_payload(&mut application, start, (end.0 + last_width - 1, end.1));

        assert_eq!(content.text, expected);
        assert_eq!(content.html.as_deref(), Some(expected_html));
    }
}

#[test]
fn markdown_selection_keeps_escaped_literal_tildes_literal_in_html() {
    let workspace = workspace_dir();
    let source = "A \\~\\~literal\\~\\~";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "A ~~literal~~");
    let content = select_transcript_payload(&mut application, start, (start.0 + 12, start.1));

    assert_eq!(content.text, source);
    assert_eq!(content.html.as_deref(), Some("<p>A ~~literal~~</p>\n"));
}

#[test]
fn markdown_selection_copies_complete_and_partial_pipe_tables() {
    let workspace = workspace_dir();
    let source = "| Name | Value | Empty |\n| :--- | ---: | :---: |\n| **alpha** | `a\\|b` | |\n| beta | left\\|right | |";
    for (first, last, last_width, expected, expected_html) in [
        (
            "Name",
            "left|right",
            14,
            "| Name | Value | Empty |\n| :--- | ---: | :---: |\n| **alpha** | `a\\|b` |  |\n| beta | left\\|right |  |",
            "<table><thead><tr><th style=\"text-align: left\">Name</th><th style=\"text-align: right\">Value</th><th style=\"text-align: center\">Empty</th></tr></thead><tbody>\n<tr><td style=\"text-align: left\"><strong>alpha</strong></td><td style=\"text-align: right\"><code>a|b</code></td><td style=\"text-align: center\"></td></tr>\n<tr><td style=\"text-align: left\">beta</td><td style=\"text-align: right\">left|right</td><td style=\"text-align: center\"></td></tr>\n</tbody></table>\n",
        ),
        (
            "alpha",
            "left|right",
            4,
            "|  |  |  |\n| :--- | ---: | :---: |\n| **alpha** | `a\\|b` |  |\n| beta | left |  |",
            "<table><thead><tr><th style=\"text-align: left\"></th><th style=\"text-align: right\"></th><th style=\"text-align: center\"></th></tr></thead><tbody>\n<tr><td style=\"text-align: left\"><strong>alpha</strong></td><td style=\"text-align: right\"><code>a|b</code></td><td style=\"text-align: center\"></td></tr>\n<tr><td style=\"text-align: left\">beta</td><td style=\"text-align: right\">left</td><td style=\"text-align: center\"></td></tr>\n</tbody></table>\n",
        ),
        (
            "left|right",
            "left|right",
            4,
            "|  |\n| ---: |\n| left |",
            "<table><thead><tr><th style=\"text-align: right\"></th></tr></thead><tbody>\n<tr><td style=\"text-align: right\">left</td></tr>\n</tbody></table>\n",
        ),
    ] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage(source)],
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 100, 32);
        let start = text_position(&buffer, first);
        let end = text_position(&buffer, last);
        let content =
            select_transcript_payload(&mut application, start, (end.0 + last_width - 1, end.1));
        assert_eq!(content.text, expected);
        assert_eq!(content.html.as_deref(), Some(expected_html));
    }
}

#[test]
fn markdown_selection_rejoins_a_wrapped_bordered_table_as_pipe_markdown() {
    let workspace = workspace_dir();
    let source =
        "| Name | Value |\n| --- | --- |\n| alpha beta gamma epsilon zeta eta theta | delta |";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 32, 24);
    let start = text_position(&buffer, "Name");
    let end = text_position(&buffer, "theta");
    let content = select_transcript_payload(&mut application, start, (end.0 + 4, end.1));

    assert_eq!(content.text, source);
    assert_eq!(
        content.html.as_deref(),
        Some(
            "<table><thead><tr><th>Name</th><th>Value</th></tr></thead><tbody>\n<tr><td>alpha beta gamma epsilon zeta eta theta</td><td>delta</td></tr>\n</tbody></table>\n"
        )
    );
}

#[test]
fn markdown_selection_keeps_code_raw_inside_a_block_and_fenced_across_prose() {
    let workspace = workspace_dir();
    let source = "Before **code**\n\n````rust\nlet ticks = \"```\";\nlet 界 = 2;\n````\n\nAfter";
    for (first, last, last_width, expected) in [
        ("ticks", "let 界", 11, "ticks = \"```\";\nlet 界 = 2;"),
        (
            "Before",
            "let 界",
            11,
            "Before **code**\n\n````rust\nlet ticks = \"```\";\nlet 界 = 2;\n````",
        ),
        (
            "ticks",
            "After",
            5,
            "````rust\nticks = \"```\";\nlet 界 = 2;\n````\n\nAfter",
        ),
    ] {
        for reverse in [false, true] {
            let mut application = connected_application(workspace.path());
            pin_release_copy(&mut application);
            application
                .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                    SessionId::new(),
                    workspace.path(),
                    &[RunEntry::AgentMessage(source)],
                )))
                .unwrap();
            let buffer = rendered_application_buffer(&application, 60, 32);
            let start = text_position(&buffer, first);
            let end = text_position(&buffer, last);
            let end = (end.0 + last_width - 1, end.1);
            let (start, end) = if reverse { (end, start) } else { (start, end) };
            assert_eq!(select_transcript(&mut application, start, end), expected);
        }
    }
}

#[test]
fn markdown_selection_unwraps_formatted_unicode_across_messages_and_omits_hidden_reasoning() {
    let workspace = workspace_dir();
    let entries = [
        RunEntry::AgentMessage(
            "## First\n\n**one two three four five six seven eight 界🙂 nine ten**",
        ),
        RunEntry::Reasoning(ReasoningBlock::thought("Hidden", "Secret **words**", 20)),
        RunEntry::AgentMessage("## Last\n\n*final words*"),
    ];
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &entries,
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 32, 40);
    let start = text_position(&buffer, "First");
    let end = text_position(&buffer, "final words");
    let content = select_transcript_payload(&mut application, start, (end.0 + 10, end.1));
    assert_eq!(
        content.html.as_deref(),
        Some(
            "<h2>First</h2>\n<p><strong>one two three four five six seven eight 界🙂 nine ten</strong></p>\n<h2>Last</h2>\n<p><em>final words</em></p>\n"
        )
    );
    assert_eq!(
        content.text,
        "## First\n\n**one two three four five six seven eight 界🙂 nine ten**\n\n## Last\n\n*final words*"
    );
}

#[test]
fn markdown_selection_preserves_visible_reasoning_and_excludes_folded_body() {
    let workspace = workspace_dir();
    let mut application = client_showing_reasoning(workspace.path());
    let mut settings = EffectiveSettings::default();
    settings.transcript.reasoning_visibility = ReasoningVisibility::Shown;
    settings.sidebar.initial_visibility = suru::protocol::SidebarVisibility::Hidden;
    settings.text_selection.copy = suru::protocol::TextSelectionCopy::Release;
    deliver_settings(
        &mut application,
        settings,
        &["textSelection.copy", "transcript.reasoningVisibility"],
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[
                RunEntry::Reasoning(ReasoningBlock::thought(
                    "Inspect",
                    "### Plan\n\nChoose **this** path",
                    20,
                )),
                RunEntry::AgentMessage("Final answer"),
            ],
        )))
        .unwrap();
    let rows = rendered_application_rows_at(&application, 80, 32);
    left_click_at(&mut application, rendered_row(&rows, "Thought") as u16).unwrap();
    let buffer = rendered_application_buffer(&application, 80, 32);
    let start = text_position(&buffer, "Plan");
    let end = text_position(&buffer, "this");
    let content = select_transcript_payload(&mut application, start, (end.0 + 3, end.1));
    assert_eq!(content.text, "### Plan\n\nChoose **this**");
    assert_eq!(
        content.html.as_deref(),
        Some("<h3>Plan</h3>\n<p>Choose <strong>this</strong></p>\n")
    );
}

#[test]
fn markdown_selection_fences_a_code_only_message_when_other_selected_messages_are_prose() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[
                RunEntry::AgentMessage("Example"),
                RunEntry::AgentMessage("```rust\nlet answer = 42;\n```"),
            ],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 32);
    let start = text_position(&buffer, "Example");
    let end = text_position(&buffer, "let answer");
    let content = select_transcript_payload(&mut application, start, (end.0 + 15, end.1));
    assert_eq!(content.text, "Example\n\n```rust\nlet answer = 42;\n```");
    assert_eq!(
        content.html.as_deref(),
        Some(
            "<p>Example</p>\n<pre><code class=\"language-rust\">let answer = 42;\n</code></pre>\n"
        )
    );
}

#[test]
fn markdown_selection_preserves_hard_breaks_inside_inline_formatting() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage("A **bold  \nnext** sentence")],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "A bold");
    let end = text_position(&buffer, "next sentence");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 12, end.1)),
        "A **bold\\\nnext** sentence"
    );
}

#[test]
fn markdown_selection_inside_a_nested_code_block_copies_only_code() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "- context\n\n  ```rust\n  let answer = 42;\n  ```",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "let answer");
    assert_eq!(
        select_transcript(&mut application, start, (start.0 + 15, start.1)),
        "let answer = 42;"
    );
}

#[test]
fn markdown_selection_keeps_code_as_a_separate_block_in_a_list_item() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "- context\n\n  ```rust\n  let answer = 42;\n  ```",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "context");
    let end = text_position(&buffer, "let answer");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 15, end.1)),
        "- context\n\n  ```rust\n  let answer = 42;\n  ```"
    );
}

#[test]
fn markdown_selection_keeps_literal_entities_and_block_markers_as_selected_text() {
    let workspace = workspace_dir();
    for (source, first, last, expected) in [
        (
            "&amp;copy; remains",
            "&copy;",
            "remains",
            "\\&copy; remains",
        ),
        ("before - literal", "- literal", "literal", "\\- literal"),
        ("before 1. literal", "1. literal", "literal", "1\\. literal"),
        (r"\<tag> literal", "<tag>", "literal", r"\<tag\> literal"),
    ] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage(source)],
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 80, 24);
        let start = text_position(&buffer, first);
        let end = text_position(&buffer, last);
        assert_eq!(
            select_transcript(
                &mut application,
                start,
                (end.0 + last.len() as u16 - 1, end.1)
            ),
            expected
        );
    }
}

#[test]
fn markdown_selection_round_trips_html_and_reordered_footnotes() {
    let workspace = workspace_dir();
    let source = "[^word]: <b>detail</b>\n\nSee <a href='https://example.test/?a=1&amp;b=2'>source</a>[^word].";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 100, 24);
    let start = text_position(&buffer, "See source");
    let end = text_position(&buffer, "detail");
    let content = select_transcript_payload(&mut application, start, (end.0 + 5, end.1));

    assert_eq!(
        content.text,
        "See [source](https://example.test/?a=1&amp;b=2)[^word].\n\n[^word]: **detail**"
    );
    let html = content.html.expect("Markdown selection carries rich HTML");
    assert!(html.contains("href=\"https://example.test/?a=1&amp;b=2\""));
    assert!(html.contains("<strong>detail</strong>"));
    assert!(html.contains("footnote-reference"));
    assert!(!html.contains("<b>"));
}

#[test]
fn markdown_selection_preserves_escaped_link_metadata() {
    let workspace = workspace_dir();
    let source = r#"[link](https://example.test/a\\ "a&amp;copy;") and [next](https://example.test/a&amp;copy;)"#;
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 160, 24);
    let start = text_position(&buffer, "link");
    let end = text_position(&buffer, "next");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 3, end.1)),
        source
    );
}

#[test]
fn supported_markdown_link_clicks_open_semantically_and_copy_as_markdown() {
    let workspace = workspace_dir();
    let source = "Read [guide](https://example.test/path).";
    let facts = TerminalFacts::default().with_hyperlinks(true);
    let mut application = connected_application_with_terminal_facts(workspace.path(), facts);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();

    let buffer = rendered_application_buffer(&application, 80, 24);
    let position = text_position(&buffer, "guide");
    assert!(
        !buffer_rows(&buffer)
            .join("\n")
            .contains("https://example.test")
    );
    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: position.0,
            row: position.1,
            modifiers: KeyModifiers::NONE,
        },
    )
    .unwrap();
    assert_eq!(
        transition,
        ApplicationTransition::OpenHyperlink("https://example.test/path".to_owned())
    );

    pin_release_copy(&mut application);
    assert_eq!(
        select_transcript(
            &mut application,
            position,
            (position.0 + "guide".len() as u16 - 1, position.1),
        ),
        "[guide](https://example.test/path)"
    );
}

#[test]
fn dragging_from_a_markdown_link_selects_instead_of_opening_it() {
    use crossterm::event::MouseButton;

    let workspace = workspace_dir();
    let mut application = connected_application_with_terminal_facts(
        workspace.path(),
        TerminalFacts::default().with_hyperlinks(true),
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "Read [guide](https://example.test/path).",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "guide");
    let end = (start.0 + 2, start.1);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let position = if matches!(kind, MouseEventKind::Down(_)) {
            start
        } else {
            end
        };
        let transition = application
            .handle_terminal_event(selection_mouse(kind, position))
            .unwrap();
        assert!(!matches!(
            transition,
            ApplicationTransition::OpenHyperlink(_)
        ));
    }
}

#[test]
fn markdown_rule_copy_is_valid_from_far_right_partial_cells_at_multiple_widths() {
    let workspace = workspace_dir();
    for width in [50, 83] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage("before\n\n---\n\nafter")],
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, width, 24);
        let before = text_position(&buffer, "before").1;
        let after = text_position(&buffer, "after").1;
        let rows = buffer_rows(&buffer);
        let (rule_row, rule) = rows
            .iter()
            .enumerate()
            .skip(usize::from(before + 1))
            .take(usize::from(after - before - 1))
            .find(|(_, row)| row.contains('─'))
            .expect("the rule is painted between its neighboring paragraphs");
        let cells = rule.chars().collect::<Vec<_>>();
        let first = cells
            .iter()
            .position(|character| *character == '─')
            .expect("rule start") as u16;
        let last = cells
            .iter()
            .rposition(|character| *character == '─')
            .expect("rule end") as u16;
        let rule_row = rule_row as u16;
        assert!(
            last > first + 2,
            "the rule spans the message at width {width}"
        );

        for start in [last, last - 2] {
            assert_eq!(
                select_transcript_payload(
                    &mut application,
                    (start, rule_row),
                    (first, rule_row + 1),
                ),
                suru::tui::ClipboardContent {
                    text: "---".into(),
                    html: Some("<hr />\n".into()),
                },
                "copying the far-right {} cell(s) at width {width}",
                last - start + 1,
            );
        }
    }
}

#[test]
fn markdown_image_chrome_is_excluded_from_plain_and_rich_copy() {
    let workspace = workspace_dir();
    let source = "![diagram](https://example.test/diagram.png)";
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 100, 24);
    let start = text_position(&buffer, "[image:");
    let destination = "https://example.test/diagram.png";
    let end = text_position(&buffer, destination);

    assert_eq!(
        select_transcript_payload(
            &mut application,
            start,
            (end.0 + destination.len() as u16 - 1, end.1),
        ),
        suru::tui::ClipboardContent {
            text: source.into(),
            html: Some("<p><a href=\"https://example.test/diagram.png\">diagram</a></p>\n".into()),
        }
    );
}

#[test]
fn supported_linked_image_copy_keeps_both_authored_destinations() {
    let workspace = workspace_dir();
    let source = "[![diagram](image.png)](https://example.test/page)";
    let mut application = connected_application_with_terminal_facts(
        workspace.path(),
        TerminalFacts::default().with_hyperlinks(true),
    );
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(source)],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 100, 24);
    let start = text_position(&buffer, "[image:");
    let end = text_position(&buffer, "diagram");

    assert_eq!(
        select_transcript(
            &mut application,
            start,
            (end.0 + "diagram".len() as u16 - 1, end.1),
        ),
        source
    );
}

#[test]
fn markdown_selection_uses_valid_fences_for_backticks_in_language_metadata() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "Example\n\n~~~~lang`meta\nlet ticks = \"~~~\";\n~~~~",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "Example");
    let end = text_position(&buffer, "let ticks");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 17, end.1)),
        "Example\n\n~~~~lang`meta\nlet ticks = \"~~~\";\n~~~~"
    );
}

#[test]
fn markdown_selection_keeps_a_leading_tilde_fence_inside_a_list() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(
                "- ~~~lang`meta\n  code\n  ~~~\n\n  after",
            )],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 80, 24);
    let start = text_position(&buffer, "code");
    let end = text_position(&buffer, "after");
    assert_eq!(
        select_transcript(&mut application, start, (end.0 + 4, end.1)),
        "- ~~~lang`meta\n  code\n  ~~~\n\n  after"
    );
}

#[test]
fn rich_selection_escapes_literal_markup_and_keeps_code_literal() {
    let workspace = workspace_dir();
    for (source, first, last, last_width, text, html) in [
        (
            r"before \<script>alert(1)\</script> after",
            "<script>",
            "</script>",
            9,
            "\\<script\\>alert(1)\\</script\\>",
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>\n",
        ),
        (
            "```html\n<b>**literal** & value</b>   \nlast\n```",
            "<b>",
            "last",
            4,
            "<b>**literal** & value</b>\nlast",
            "<pre><code>&lt;b&gt;**literal** &amp; value&lt;/b&gt;\nlast</code></pre>\n",
        ),
        (
            "[chosen](javascript:alert%281%29) ![picture](https://example.test/image.png)",
            "chosen",
            "picture",
            7,
            "[chosen](javascript:alert%281%29) ![picture](https://example.test/image.png)",
            "<p><a href=\"\">chosen</a> <a href=\"https://example.test/image.png\">picture</a></p>\n",
        ),
    ] {
        let mut application = connected_application(workspace.path());
        pin_release_copy(&mut application);
        application
            .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
                SessionId::new(),
                workspace.path(),
                &[RunEntry::AgentMessage(source)],
            )))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 100, 32);
        let start = text_position(&buffer, first);
        let end = text_position(&buffer, last);
        let content =
            select_transcript_payload(&mut application, start, (end.0 + last_width - 1, end.1));
        assert_eq!(content.text, text, "{source}");
        assert_eq!(content.html.as_deref(), Some(html), "{source}");
    }
}

/// An Application with one agent Message, driven by a clock the test advances.
fn word_click_application(content: &'static str) -> (Application, Arc<AtomicU64>) {
    let workspace = workspace_dir();
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let observed_elapsed_ms = Arc::clone(&elapsed_ms);
    let origin = Instant::now();
    let mut application = Application::new(workspace.path(), Default::default())
        .with_presentation_clock(move || {
            origin + Duration::from_millis(observed_elapsed_ms.load(Ordering::Relaxed))
        });
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[RunEntry::AgentMessage(content)],
        )))
        .unwrap();
    (application, elapsed_ms)
}

/// A press and release at one cell; the release's transition.
fn click_at(application: &mut Application, position: (u16, u16)) -> ApplicationTransition {
    use crossterm::event::MouseButton;
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(MouseButton::Left),
            position,
        ))
        .unwrap();
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(MouseButton::Left),
            position,
        ))
        .unwrap()
}

/// Two clicks at one cell within the click interval; the second release's
/// transition.
fn double_click_at(
    application: &mut Application,
    clock: &AtomicU64,
    position: (u16, u16),
) -> ApplicationTransition {
    click_at(application, position);
    clock.fetch_add(100, Ordering::Relaxed);
    click_at(application, position)
}

fn reversed_columns(buffer: &Buffer, row: u16) -> Vec<u16> {
    (0..buffer.area.width)
        .filter(|&x| buffer[(x, row)].modifier.contains(Modifier::REVERSED))
        .collect()
}

fn copied_text(transition: &ApplicationTransition) -> Option<&str> {
    match transition {
        ApplicationTransition::CopyToClipboard(content) => Some(content.text.as_str()),
        _ => None,
    }
}

#[test]
fn double_click_highlights_and_copies_the_word_under_the_pointer() {
    let (mut application, clock) = word_click_application("Alpha bravo charlie");
    let buffer = rendered_application_buffer(&application, 60, 24);
    let bravo = text_position(&buffer, "bravo");
    let first = click_at(&mut application, (bravo.0 + 2, bravo.1));
    assert_eq!(first, ApplicationTransition::Continue);
    assert!(
        reversed_columns(&rendered_application_buffer(&application, 60, 24), bravo.1).is_empty()
    );
    clock.fetch_add(100, Ordering::Relaxed);
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            (bravo.0 + 2, bravo.1),
        ))
        .unwrap();
    assert_eq!(
        reversed_columns(&rendered_application_buffer(&application, 60, 24), bravo.1),
        (bravo.0..bravo.0 + 5).collect::<Vec<_>>(),
        "the word highlights on the press"
    );
    let released = application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            (bravo.0 + 2, bravo.1),
        ))
        .unwrap();
    assert_eq!(copied_text(&released), Some("bravo"), "{released:?}");
    assert_eq!(
        reversed_columns(&rendered_application_buffer(&application, 60, 24), bravo.1),
        (bravo.0..bravo.0 + 5).collect::<Vec<_>>(),
        "the highlight stands after a release copy"
    );
}

fn press_at(application: &mut Application, position: (u16, u16)) {
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            position,
        ))
        .unwrap();
}

fn reversed_cells(buffer: &Buffer) -> Vec<(u16, u16)> {
    let mut cells = Vec::new();
    for y in 0..buffer.area.height {
        for x in reversed_columns(buffer, y) {
            cells.push((x, y));
        }
    }
    cells
}

fn word_cells((x, y): (u16, u16), width: u16) -> Vec<(u16, u16)> {
    (x..x + width).map(|x| (x, y)).collect()
}

#[test]
fn second_press_one_cell_away_within_the_interval_continues_the_count() {
    let (mut application, clock) = word_click_application("Alpha bravo  \ncharlie delta");
    let buffer = rendered_application_buffer(&application, 60, 24);
    let bravo = text_position(&buffer, "bravo");
    let charlie = text_position(&buffer, "charlie");
    assert_eq!(
        charlie.1,
        bravo.1 + 1,
        "the hard break puts charlie one Row below"
    );
    click_at(&mut application, bravo);
    clock.fetch_add(499, Ordering::Relaxed);
    let released = click_at(&mut application, (bravo.0 + 1, bravo.1));
    assert_eq!(copied_text(&released), Some("bravo"));
    assert_eq!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)),
        word_cells(bravo, 5)
    );
    clock.fetch_add(500, Ordering::Relaxed);
    click_at(&mut application, (charlie.0 + 2, bravo.1));
    clock.fetch_add(100, Ordering::Relaxed);
    let released = click_at(&mut application, (charlie.0 + 2, charlie.1));
    assert_eq!(
        copied_text(&released),
        Some("charlie"),
        "one Row away still counts"
    );
    assert_eq!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)),
        word_cells(charlie, 7)
    );
}

#[test]
fn second_press_two_cells_away_after_the_interval_or_on_another_surface_starts_over() {
    let (mut application, clock) = word_click_application("Alpha bravo charlie");
    let buffer = rendered_application_buffer(&application, 60, 24);
    let bravo = text_position(&buffer, "bravo");
    let composer = text_position(&buffer, "Type a prompt");
    click_at(&mut application, bravo);
    clock.fetch_add(100, Ordering::Relaxed);
    let released = click_at(&mut application, (bravo.0 + 2, bravo.1));
    assert_eq!(released, ApplicationTransition::Continue, "two cells away");
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());

    clock.fetch_add(1_000, Ordering::Relaxed);
    click_at(&mut application, bravo);
    clock.fetch_add(501, Ordering::Relaxed);
    let released = click_at(&mut application, bravo);
    assert_eq!(
        released,
        ApplicationTransition::Continue,
        "after the interval"
    );
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());

    clock.fetch_add(1_000, Ordering::Relaxed);
    click_at(&mut application, composer);
    clock.fetch_add(100, Ordering::Relaxed);
    click_at(&mut application, bravo);
    assert!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty(),
        "a press on the composer then one on the Transcript is a fresh count"
    );
    clock.fetch_add(100, Ordering::Relaxed);
    let released = click_at(&mut application, bravo);
    assert_eq!(
        copied_text(&released),
        Some("bravo"),
        "and the Transcript count then continues"
    );
}

#[test]
fn double_click_in_manual_mode_stands_until_ctrl_c_copies_it() {
    let (mut application, clock) = word_click_application("Alpha bravo charlie");
    let mut settings = EffectiveSettings::default();
    settings.text_selection.copy = suru::protocol::TextSelectionCopy::Manual;
    deliver_settings(&mut application, settings, &["textSelection.copy"]);
    let buffer = rendered_application_buffer(&application, 60, 24);
    let bravo = text_position(&buffer, "bravo");
    let released = double_click_at(&mut application, &clock, bravo);
    assert_eq!(released, ApplicationTransition::Continue);
    assert_eq!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)),
        word_cells(bravo, 5)
    );
    let copied = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    assert_eq!(copied_text(&copied), Some("bravo"));
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
}

#[test]
fn double_click_selects_whole_tokens_and_runs_by_class() {
    for (content, needle, offset, highlighted, expected) in [
        (
            "See src/tui/state.rs now",
            "tui",
            0,
            ("src/tui/state.rs", 16, 1),
            "src/tui/state.rs",
        ),
        (
            "Open https://example.com/a?b=1 today",
            "example",
            3,
            ("https://example.com/a?b=1", 25, 1),
            "https://example.com/a?b=1",
        ),
        (
            "```sh\ncargo run --dry-run\n```",
            "dry",
            0,
            ("--dry-run", 9, 1),
            "--dry-run",
        ),
        (
            "Say 'quoted' aloud",
            "quoted",
            2,
            ("quoted", 6, 1),
            "quoted",
        ),
        ("Call foo(); now", "(", 0, ("();", 3, 1), "();"),
        ("abc漢字テストxyz", "漢", 2, ("漢", 5, 2), "漢字テスト"),
        ("ok 🙂 go", "🙂", 1, ("🙂", 1, 2), "🙂"),
    ] {
        let (mut application, clock) = word_click_application(content);
        let buffer = rendered_application_buffer(&application, 60, 24);
        let target = text_position(&buffer, needle);
        let released = double_click_at(&mut application, &clock, (target.0 + offset, target.1));
        assert_eq!(
            copied_text(&released),
            Some(expected),
            "{content}: {released:?}"
        );
        let (x, y) = text_position(&buffer, highlighted.0);
        assert_eq!(
            reversed_cells(&rendered_application_buffer(&application, 60, 24)),
            (0..highlighted.1)
                .map(|cell| (x + cell * highlighted.2, y))
                .collect::<Vec<_>>(),
            "{content}"
        );
    }
}

#[test]
fn double_click_on_a_run_of_spaces_highlights_the_run() {
    let (mut application, clock) = word_click_application("```text\nfoo   bar\n```");
    let buffer = rendered_application_buffer(&application, 60, 24);
    let foo = text_position(&buffer, "foo");
    double_click_at(&mut application, &clock, (foo.0 + 4, foo.1));
    assert_eq!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)),
        word_cells((foo.0 + 3, foo.1), 3)
    );
}

#[test]
fn double_click_on_a_soft_wrapped_word_highlights_both_rows_and_copies_it_whole() {
    let token = "abcdefghij".repeat(5);
    let content: &'static str = Box::leak(format!("start {token} end").into_boxed_str());
    let (mut application, clock) = word_click_application(content);
    let buffer = rendered_application_buffer(&application, 40, 24);
    let start = text_position(&buffer, "start");
    let head = text_position(&buffer, "abcdefghij");
    let released = double_click_at(&mut application, &clock, head);
    assert_eq!(copied_text(&released), Some(token.as_str()), "{released:?}");
    let highlighted = reversed_cells(&rendered_application_buffer(&application, 40, 24));
    let rows: std::collections::BTreeSet<u16> = highlighted.iter().map(|&(_, y)| y).collect();
    assert_eq!(rows.len(), 2, "{highlighted:?}");
    assert_eq!(highlighted.len(), 50, "{highlighted:?}");
    assert!(!highlighted.contains(&start));
}

#[test]
fn double_click_on_chrome_or_a_separator_selects_nothing_and_clicks_as_before() {
    let (mut application, clock) = word_click_application("```rust\nlet answer = 42;\n```");
    let buffer = rendered_application_buffer(&application, 60, 24);
    let info = text_position(&buffer, "rust");
    assert_eq!(
        double_click_at(&mut application, &clock, info),
        ApplicationTransition::Continue
    );
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());

    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::clone(&elapsed_ms);
    let origin = Instant::now();
    let mut application =
        connected_application(workspace.path()).with_presentation_clock(move || {
            origin + Duration::from_millis(clock.load(Ordering::Relaxed))
        });
    pin_release_copy(&mut application);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let rows = rendered_application_rows_at(&application, 60, 24);
    let header = rendered_row(&rows, "✓ cargo test") as u16;
    let separator = (4, header - 1);
    assert_eq!(rows[usize::from(header - 1)].trim(), "");
    assert_eq!(
        double_click_at(&mut application, &elapsed_ms, separator),
        ApplicationTransition::Continue
    );
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());

    elapsed_ms.fetch_add(1_000, Ordering::Relaxed);
    let marker = (rows[usize::from(header)].find('✓').unwrap() as u16, header);
    press_at(&mut application, marker);
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
    click_release_at(&mut application, marker);
    let peek = rendered_application_rows_at(&application, 60, 24);
    assert!(
        peek.join("\n").contains("… +6 lines"),
        "first click opens the Peek"
    );
    elapsed_ms.fetch_add(100, Ordering::Relaxed);
    press_at(&mut application, marker);
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
    click_release_at(&mut application, marker);
    assert!(
        !rendered_application_rows_at(&application, 60, 24)
            .join("\n")
            .contains("… +6 lines"),
        "the second click on the Marker column folds again"
    );

    elapsed_ms.fetch_add(1_000, Ordering::Relaxed);
    click_at(&mut application, marker);
    let peek = rendered_application_rows_at(&application, 60, 24);
    let fold_marker = (
        peek[rendered_row(&peek, "… +6 lines")].find('…').unwrap() as u16 + 2,
        rendered_row(&peek, "… +6 lines") as u16,
    );
    elapsed_ms.fetch_add(1_000, Ordering::Relaxed);
    press_at(&mut application, fold_marker);
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
    assert_eq!(
        click_release_at(&mut application, fold_marker),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows_at(&application, 60, 24)
            .join("\n")
            .contains("output line 1 "),
        "the fold marker click expands as before"
    );
}

fn click_release_at(application: &mut Application, position: (u16, u16)) -> ApplicationTransition {
    application
        .handle_terminal_event(selection_mouse(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            position,
        ))
        .unwrap()
}

#[test]
fn click_count_survives_a_release_copy_ctrl_c_escape_and_a_fold_toggle() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    for reset in ["release", "ctrl-c", "escape", "fold"] {
        let elapsed_ms = Arc::new(AtomicU64::new(0));
        let clock = Arc::clone(&elapsed_ms);
        let origin = Instant::now();
        let mut application =
            connected_application(workspace.path()).with_presentation_clock(move || {
                origin + Duration::from_millis(clock.load(Ordering::Relaxed))
            });
        let mut settings = EffectiveSettings::default();
        settings.text_selection.copy = if reset == "ctrl-c" {
            suru::protocol::TextSelectionCopy::Manual
        } else {
            suru::protocol::TextSelectionCopy::Release
        };
        deliver_settings(&mut application, settings, &["textSelection.copy"]);
        application
            .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
            .unwrap();
        let buffer = rendered_application_buffer(&application, 60, 24);
        let suite = text_position(&buffer, "suite");
        double_click_at(&mut application, &elapsed_ms, suite);
        assert_eq!(
            reversed_cells(&rendered_application_buffer(&application, 60, 24)),
            word_cells(suite, 5),
            "{reset}"
        );
        let event = match reset {
            "ctrl-c" => Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
            "escape" => Some(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))),
            _ => None,
        };
        if let Some(event) = event {
            application.handle_terminal_event(event).unwrap();
            assert!(
                reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty(),
                "{reset} clears the selection"
            );
        }
        if reset == "fold" {
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    SemanticCommandId::TranscriptFoldsToggle,
                )))
                .unwrap();
        }
        elapsed_ms.fetch_add(100, Ordering::Relaxed);
        press_at(&mut application, suite);
        assert_eq!(
            reversed_cells(&rendered_application_buffer(&application, 60, 24)),
            word_cells(text_position(&buffer, "Run the test suite"), 18),
            "{reset}: the third press selects the Line"
        );
        let released = click_release_at(&mut application, suite);
        let copied = if reset == "ctrl-c" {
            assert_eq!(released, ApplicationTransition::Continue);
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )))
                .unwrap()
        } else {
            released
        };
        assert_eq!(copied_text(&copied), Some("Run the test suite"));
    }
}

#[test]
fn the_word_gesture_is_a_semantic_command() {
    assert_eq!(
        SemanticCommandId::TextSelectionWord.as_str(),
        "text_selection.word"
    );
}

#[test]
fn zz_probe_fold_marker() {
    let workspace = workspace_dir();
    let (snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let rows = rendered_application_rows_at(&application, 60, 24);
    left_click_at(&mut application, rendered_row(&rows, "✓ cargo test") as u16).unwrap();
    let peek = rendered_application_rows_at(&application, 60, 24);
    for (i, r) in peek.iter().enumerate() {
        eprintln!("{i:2}|{r}|");
    }
    left_click_at(&mut application, rendered_row(&peek, "… +6 lines") as u16).unwrap();
    for (i, r) in rendered_application_rows_at(&application, 60, 24)
        .iter()
        .enumerate()
    {
        eprintln!("{i:2}|{r}|");
    }
}

#[test]
fn triple_click_selects_a_wrapped_line_and_fourth_click_keeps_it() {
    let (mut application, clock) =
        word_click_application("Alpha bravo charlie delta echo foxtrot golf hotel india juliet");
    let buffer = rendered_application_buffer(&application, 40, 24);
    let start = text_position(&buffer, "Alpha");
    let end = text_position(&buffer, "juliet");
    let bravo = text_position(&buffer, "bravo");
    assert!(end.1 > start.1);
    double_click_at(&mut application, &clock, bravo);
    for _ in 0..2 {
        clock.fetch_add(100, Ordering::Relaxed);
        press_at(&mut application, bravo);
        let selected = rendered_application_buffer(&application, 40, 24);
        assert!(selected[start].modifier.contains(Modifier::REVERSED));
        assert!(
            selected[(end.0 + 5, end.1)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        for y in start.1..=end.1 {
            assert!(!reversed_columns(&selected, y).is_empty());
            assert!(
                !selected[(start.0 - 1, y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
        assert_eq!(
            copied_text(&click_release_at(&mut application, bravo)),
            Some("Alpha bravo charlie delta echo foxtrot golf hotel india juliet")
        );
        assert_eq!(
            click_release_at(&mut application, bravo),
            ApplicationTransition::Continue
        );
    }
}

fn triple_click_at(
    application: &mut Application,
    clock: &AtomicU64,
    position: (u16, u16),
) -> ApplicationTransition {
    double_click_at(application, clock, position);
    clock.fetch_add(100, Ordering::Relaxed);
    click_at(application, position)
}

#[test]
fn triple_click_copies_code_indentation_and_markdown_shape_without_chrome() {
    for (source, needle, offset, expected, html, highlight) in [
        (
            "```rust\n    let answer = 42;   \nnext\n```",
            "let answer",
            0,
            "    let answer = 42;",
            "<pre><code>    let answer = 42;</code></pre>\n",
            (4, 20),
        ),
        (
            "Alpha **bravo** charlie",
            "bravo",
            0,
            "Alpha **bravo** charlie",
            "<p>Alpha <strong>bravo</strong> charlie</p>\n",
            (6, 19),
        ),
    ] {
        let (mut application, clock) = word_click_application(source);
        let buffer = rendered_application_buffer(&application, 60, 24);
        let target = text_position(&buffer, needle);
        let copied = triple_click_at(&mut application, &clock, (target.0 + offset, target.1));
        assert_eq!(copied_text(&copied), Some(expected));
        let ApplicationTransition::CopyToClipboard(content) = copied else {
            panic!("no copy")
        };
        assert_eq!(content.html.as_deref(), Some(html));
        assert_eq!(
            reversed_cells(&rendered_application_buffer(&application, 60, 24)),
            word_cells((target.0 - highlight.0, target.1), highlight.1)
        );
    }
}

#[test]
fn triple_click_on_blank_lines_and_separators_copies_an_empty_line() {
    for source in ["before\n\nafter", "```text\nbefore\n   \nafter\n```"] {
        let (mut application, clock) = word_click_application(source);
        let buffer = rendered_application_buffer(&application, 60, 24);
        let before = text_position(&buffer, "before");
        let after = text_position(&buffer, "after");
        assert_eq!(after.1, before.1 + 2);
        let copied = triple_click_at(&mut application, &clock, (before.0, before.1 + 1));
        assert_eq!(copied_text(&copied), Some(""));
        assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
    }
}

#[test]
fn triple_click_selects_every_part_of_an_oversize_line() {
    let source: &'static str =
        Box::leak(format!("START {} END", "x".repeat(36_000)).into_boxed_str());
    let (mut application, clock) = word_click_application(source);
    let buffer = rendered_application_buffer(&application, 32, 1600);
    let start = text_position(&buffer, "START");
    let end = text_position(&buffer, "END");
    let copied = triple_click_at(&mut application, &clock, end);
    assert_eq!(copied_text(&copied), Some(source));
    let selected = rendered_application_buffer(&application, 32, 1600);
    for y in start.1..=end.1 {
        assert!(!reversed_columns(&selected, y).is_empty(), "Row {y}");
    }
    assert!(selected[start].modifier.contains(Modifier::REVERSED));
    assert!(
        selected[(end.0 + 2, end.1)]
            .modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn triple_click_copies_command_and_fold_words_without_affordances() {
    let workspace = workspace_dir();
    let (mut snapshot, _) = command_activity_session(
        workspace.path(),
        ActivityStatus::Completed,
        &numbered_output(12),
        false,
    );
    if let Activity::Command { command, .. } = &mut snapshot.activities[0] {
        *command = "   cargo test   ".into();
    }
    let (mut application, clock) = word_click_application("placeholder");
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let command = text_position(&buffer, "cargo test");
    let separator = (command.0, command.1 - 1);
    assert_eq!(
        copied_text(&triple_click_at(&mut application, &clock, separator)),
        Some("")
    );
    assert!(reversed_cells(&rendered_application_buffer(&application, 60, 24)).is_empty());
    clock.fetch_add(1_000, Ordering::Relaxed);
    assert_eq!(
        copied_text(&triple_click_at(&mut application, &clock, command)),
        Some("cargo test")
    );
    assert_eq!(
        reversed_cells(&rendered_application_buffer(&application, 60, 24)),
        word_cells(command, 10)
    );
    let mut settings = EffectiveSettings::default();
    settings.transcript.reasoning_visibility = ReasoningVisibility::Shown;
    settings.text_selection.copy = suru::protocol::TextSelectionCopy::Release;
    deliver_settings(
        &mut application,
        settings,
        &["textSelection.copy", "transcript.reasoningVisibility"],
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(command_run_snapshot(
            SessionId::new(),
            workspace.path(),
            &[
                RunEntry::Reasoning(ReasoningBlock::thought("Inspect the files", "Details", 20)),
                RunEntry::AgentMessage("Final answer"),
            ],
        )))
        .unwrap();
    let buffer = rendered_application_buffer(&application, 60, 24);
    let fold = text_position(&buffer, "Inspect the files");
    clock.fetch_add(1_000, Ordering::Relaxed);
    let copied = triple_click_at(&mut application, &clock, fold);
    assert_eq!(
        copied_text(&copied),
        Some("Thought: Inspect the files · 20ms")
    );
    let selected = rendered_application_buffer(&application, 60, 24);
    assert_eq!(
        reversed_cells(&selected),
        word_cells(text_position(&buffer, "Thought"), 33)
    );
}

#[test]
fn the_line_gesture_is_a_semantic_command() {
    assert_eq!(
        SemanticCommandId::TextSelectionLine.as_str(),
        "text_selection.line"
    );
}
