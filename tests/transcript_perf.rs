//! Timing harness for transcript rendering hot paths.
//!
//! Run with: cargo test --release --test transcript_perf -- --ignored --nocapture

use std::time::Instant;

use ratatui::{Terminal, backend::TestBackend};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
        ModelAvailability, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, Session,
        SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStatus, SessionUpdate,
        TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    tui::{Application, ApplicationEvent},
};

const SECTIONS: usize = 150;

fn agent_markdown(section: usize) -> String {
    format!(
        "## Section {section}\n\nHere is a paragraph with **bold**, _emphasis_, and `inline code` \
         explaining step {section} of the work in enough prose to wrap across several terminal \
         rows at typical widths.\n\n- first bullet with detail\n- second bullet with detail\n- \
         third bullet with detail\n\n```rust\nfn example_{section}() -> usize {{\n    // \
         representative code block content\n    {section} * 42\n}}\n```\n\nClosing paragraph for \
         section {section} that also wraps across the viewport width."
    )
}

fn command_output() -> String {
    (0..40)
        .map(|line| format!("build output line {line}: compiling module and linking artifacts"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn session_snapshot(workspace: &std::path::Path, sections: usize) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot {
        title: String::new(),
        emoji: None,
        session: Session {
            context_fill: None,
            id: SessionId::new(),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: Vec::new(),
        messages: Vec::new(),
        activities: Vec::new(),
        transcript: Vec::new(),
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
    };
    for section in 1..=sections {
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let user_message_id = MessageId::new();
        let agent_message_id = MessageId::new();
        let activity_id = ActivityId::new();
        snapshot.prompts.push(Prompt {
            id: prompt_id,
            text: format!("Prompt section {section}"),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(section as u64),
            status: PromptStatus::Delivered,
            skill_invocations: Vec::new(),
        });
        snapshot.turns.push(Turn {
            id: turn_id,
            prompt_id: Some(prompt_id),
            agent: None,
            status: TurnStatus::Completed,
            started_at: None,
            settled_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
        });
        snapshot.messages.extend([
            Message {
                id: user_message_id,
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: format!("User question for section {section} with a bit of extra text"),
                skill_invocations: Vec::new(),
                truncated: false,
            },
            Message {
                id: agent_message_id,
                turn_id,
                role: MessageRole::Agent,
                status: MessageStatus::Completed,
                content: agent_markdown(section),
                skill_invocations: Vec::new(),
                truncated: false,
            },
        ]);
        snapshot.activities.push(Activity::Command {
            id: activity_id,
            turn_id,
            status: ActivityStatus::Completed,
            command: format!("cargo build --package section-{section}"),
            cwd: None,
            output: command_output(),
            output_truncated: false,
            exit_status: Some(0),
        });
        snapshot.transcript.extend([
            TranscriptItem::Message {
                message_id: user_message_id,
            },
            TranscriptItem::Activity { activity_id },
            TranscriptItem::Message {
                message_id: agent_message_id,
            },
        ]);
    }
    snapshot
}

fn streaming_update(snapshot: &SessionSnapshot, ordinal: u64) -> SessionUpdate {
    let last_agent_message = snapshot
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Agent)
        .expect("snapshot contains an Agent message");
    SessionUpdate {
        session_id: snapshot.session.id,
        revision: SessionRevision(snapshot.revision.0 + ordinal),
        changes: vec![SessionChange::MessageContentAppended {
            message_id: last_agent_message.id,
            content: " another streamed token batch".to_owned(),
        }],
    }
}

fn timed(label: &str, iterations: u32, mut body: impl FnMut()) {
    let start = Instant::now();
    for _ in 0..iterations {
        body();
    }
    let total = start.elapsed();
    println!(
        "{label}: {:.3} ms/iter ({iterations} iters, {:.1} ms total)",
        total.as_secs_f64() * 1000.0 / f64::from(iterations),
        total.as_secs_f64() * 1000.0
    );
}

#[test]
#[ignore = "timing harness, run manually with --release"]
fn transcript_render_timings() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = session_snapshot(workspace.path(), SECTIONS);
    // Streaming appends must target a streaming message.
    snapshot
        .messages
        .iter_mut()
        .rev()
        .find(|message| message.role == MessageRole::Agent)
        .map(|message| message.status = MessageStatus::Streaming)
        .expect("snapshot contains an Agent message");
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach large Session");

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("create test terminal");

    timed("cold render (first frame)", 1, || {
        terminal
            .draw(|frame| application.render(frame))
            .expect("render frame");
    });
    timed("warm render (unchanged state)", 50, || {
        terminal
            .draw(|frame| application.render(frame))
            .expect("render frame");
    });
    timed("scroll event + render", 50, || {
        application
            .handle_event(ApplicationEvent::Command(
                suru::tui::CommandId::ScrollTranscriptPageUp,
            ))
            .expect("scroll");
        terminal
            .draw(|frame| application.render(frame))
            .expect("render frame");
    });
    let mut ordinal = 1;
    timed("streaming append + render", 50, || {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                streaming_update(&snapshot, ordinal),
            )))
            .expect("apply streamed update");
        ordinal += 1;
        terminal
            .draw(|frame| application.render(frame))
            .expect("render frame");
    });
}

/// Includes applying each delta, Markdown parsing/highlighting/wrapping, and drawing.
/// Warm the grammar first: its one-time initialization is not per-delta work.
#[test]
#[ignore = "timing harness, run manually with --release"]
fn streaming_code_block_render_timings() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path(), Default::default());
    let mut snapshot = session_snapshot(workspace.path(), 1);
    let message = snapshot.messages.last_mut().expect("Agent message");
    message.status = MessageStatus::Streaming;
    message.content = "Here is the implementation:\n\n```rust\nfn example() {\n".to_owned();
    let message_id = message.id;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach Session");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("create terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("warm grammar");

    let deltas: Vec<_> = (0..24)
        .map(|n| format!("    let value_{n} = \"hello\"; // streamed line\n"))
        .chain(["}\n".to_owned(), "```".to_owned()])
        .collect();
    let mut elapsed = std::time::Duration::ZERO;
    for (index, delta) in deltas.iter().enumerate() {
        let start = Instant::now();
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
                SessionUpdate {
                    session_id: snapshot.session.id,
                    revision: SessionRevision(snapshot.revision.0 + index as u64 + 1),
                    changes: vec![SessionChange::MessageContentAppended {
                        message_id,
                        content: delta.clone(),
                    }],
                },
            )))
            .expect("apply code delta");
        terminal
            .draw(|frame| application.render(frame))
            .expect("render code delta");
        elapsed += start.elapsed();
    }
    let average_ms = elapsed.as_secs_f64() * 1000.0 / deltas.len() as f64;
    println!(
        "streaming Code Block: {average_ms:.3} ms/delta ({} deltas, {} appended bytes, 120x40)",
        deltas.len(),
        deltas.iter().map(String::len).sum::<usize>()
    );
    assert!(
        average_ms < 4.0,
        "per-delta rendering should use under a quarter of a 60 Hz frame: {average_ms:.3} ms"
    );
}
