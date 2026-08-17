//! Ratatui view state and terminal lifecycle.

use std::io::{Stdout, stdout};

use anyhow::{Result, anyhow};
use crossterm::{
    cursor::{Hide, Show},
    event::{Event as InputEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    managed_client::{ManagedClient, ManagedEvent, RecoveryStatus},
    protocol::Health,
};

#[derive(Clone, Debug, Default)]
pub struct TuiState {
    identity: Option<Health>,
    pending_identity: Option<Health>,
    counter: Option<u64>,
    recovery: Option<RecoveryStatus>,
    fatal_error: Option<String>,
}

impl TuiState {
    pub fn apply(&mut self, event: ManagedEvent) {
        match event {
            ManagedEvent::Connecting => {
                self.identity = None;
                self.pending_identity = None;
                self.counter = None;
                self.recovery = None;
                self.fatal_error = None;
            }
            ManagedEvent::Connected(identity) => {
                self.pending_identity = Some(identity);
                self.fatal_error = None;
            }
            ManagedEvent::Snapshot(snapshot) => {
                if self
                    .pending_identity
                    .as_ref()
                    .is_some_and(|identity| identity.instance_id == snapshot.instance_id)
                {
                    self.identity = self.pending_identity.take();
                }
                self.counter = Some(snapshot.value);
                self.recovery = None;
            }
            ManagedEvent::CounterUpdated(update) => self.counter = Some(update.value),
            ManagedEvent::Recovering(status) => {
                self.recovery = Some(status);
                self.fatal_error = None;
            }
            ManagedEvent::ServerShutdown(_) => self.recovery = None,
            ManagedEvent::Fatal(error) => self.fatal_error = Some(error),
        }
    }
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let [main, status_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let panel = centered_rect(main, 36, 7);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Chidori Counter ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let counter = state
        .counter
        .map_or_else(|| "--".to_owned(), |value| value.to_string());
    let counter_area = Rect::new(inner.x, inner.y + inner.height / 2, inner.width, 1);
    frame.render_widget(
        Paragraph::new(counter)
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD)),
        counter_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(status_text(state)))
            .alignment(Alignment::Center)
            .style(status_style(state)),
        status_area,
    );
}

pub async fn run(client: ManagedClient) -> Result<()> {
    let mut session = TerminalSession::enter()?;
    run_loop(&mut session.terminal, client).await
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut client: ManagedClient,
) -> Result<()> {
    let mut state = TuiState::default();
    let mut input = EventStream::new();

    loop {
        terminal.draw(|frame| render(frame, &state))?;
        tokio::select! {
            managed_event = client.next() => {
                match managed_event {
                    Some(ManagedEvent::Fatal(error)) => return Err(anyhow!(error)),
                    Some(ManagedEvent::ServerShutdown(_)) => return Ok(()),
                    Some(event) => state.apply(event),
                    None => return Err(anyhow!("managed client stopped unexpectedly")),
                }
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(InputEvent::Key(key))) if is_quit(key) => {
                        return Ok(());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn is_quit(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    matches!(key.code, KeyCode::Char('q'))
        || (matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn status_text(state: &TuiState) -> String {
    if let Some(error) = &state.fatal_error {
        return format!("Connection failed: {error}");
    }
    if let Some(recovery) = state.recovery {
        let last_server = state.identity.as_ref().map_or_else(
            || "no previous server".to_owned(),
            |identity| format!("last server pid {}", identity.pid),
        );
        return format!(
            "Recovering (attempt {}, retry in {:?}) | {last_server}",
            recovery.attempt, recovery.retry_in
        );
    }
    match &state.identity {
        Some(identity) => format!(
            "Connected | pid {} | server {}",
            identity.pid,
            &identity.instance_id.to_string()[..8]
        ),
        None => "Connecting to Chidori server...".to_owned(),
    }
}

fn status_style(state: &TuiState) -> Style {
    if state.fatal_error.is_some() {
        Style::default().fg(Color::Red)
    } else if state.identity.is_some() && state.recovery.is_none() {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Yellow)
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut output = stdout();
                let _ = execute!(output, LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}
