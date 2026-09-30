//! Client-local Approval focus and the shared words used by its panel and row.

use super::{commands::SemanticCommandId, list_window::ListWindow, state::CommandId};
use crate::{
    protocol::{
        Activity, Approval, ApprovalId, ApprovalOutcome, ApprovalSubject, CommandAction, Decision,
        SessionReference, SessionSnapshot,
    },
    theme::Theme,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};

const DECISIONS: [(Decision, &str); 4] = [
    (Decision::Accept, "Accept once"),
    (Decision::AcceptForSession, "Accept for Session"),
    (Decision::Decline, "Decline"),
    (Decision::DeclineAndInterrupt, "Decline and Interrupt"),
];

#[derive(Clone, Debug, Default)]
pub(super) struct ApprovalPanel {
    visible: Option<(SessionReference, ApprovalId)>,
    /// Which Decision the reader is on, if any. A panel the reader asked for
    /// starts on the first; one that presented itself starts on none, so the
    /// key that would confirm a choice has no choice to confirm.
    selected: Option<usize>,
    submitting: bool,
    awaiting_confirmation: bool,
    /// The window over the Decisions, which a terminal too short for all four
    /// shows a few of.
    window: ListWindow,
}

impl ApprovalPanel {
    pub(super) fn open(&mut self, session: SessionReference, id: ApprovalId) {
        self.show(session, id, Some(0));
    }

    /// Opens the panel the reader did not ask for, with nothing chosen.
    pub(super) fn present(&mut self, session: SessionReference, id: ApprovalId) {
        self.show(session, id, None);
    }

    fn show(&mut self, session: SessionReference, id: ApprovalId, selected: Option<usize>) {
        if self.visible.as_ref() != Some(&(session.clone(), id)) {
            self.selected = selected;
            self.submitting = false;
            self.awaiting_confirmation = false;
            self.window.open();
        } else if selected.is_some() && self.selected.is_none() {
            // The reader asked for the Approval the panel had presented on
            // its own, which is asking for the choice it opened without.
            self.selected = selected;
            self.window.reveal();
        }
        self.visible = Some((session, id));
    }

    /// Whether this Client has sent a Decision it has not seen confirmed.
    /// Such a wait is left alone: Esc neither hides the panel nor dismisses
    /// anything while it stands, because the Decision is already on its way
    /// and a second one would be a second delivery. The panel reconciles
    /// closed when the Server settles it.
    pub(super) fn awaits_confirmation(&self) -> bool {
        self.awaiting_confirmation
    }

    pub(super) fn hide(&mut self) {
        self.visible = None;
        self.submitting = false;
        self.awaiting_confirmation = false;
    }

    pub(super) fn is_open(&self, session: Option<&SessionReference>) -> bool {
        self.visible
            .as_ref()
            .is_some_and(|(owner, _)| Some(owner) == session)
    }

    pub(super) fn id(&self) -> Option<ApprovalId> {
        self.visible.as_ref().map(|(_, id)| *id)
    }

    pub(super) fn reconcile(&mut self, session: &SessionReference, snapshot: &SessionSnapshot) {
        let Some((owner, id)) = &self.visible else {
            return;
        };
        if owner != session {
            return;
        }
        let outcome = snapshot
            .activities
            .iter()
            .find_map(|activity| match activity {
                Activity::Approval {
                    approval, outcome, ..
                } if approval.id == *id => Some(*outcome),
                _ => None,
            });
        match outcome {
            Some(ApprovalOutcome::Submitting) => self.submitting = true,
            Some(ApprovalOutcome::Pending) if self.awaiting_confirmation => {}
            Some(ApprovalOutcome::Pending | ApprovalOutcome::SubmissionRejected) => {
                self.submitting = false
            }
            _ => self.hide(),
        }
    }

    /// Reconciles the authoritative read performed after this client's
    /// submission attempt. Unlike an ordinary potentially stale stream
    /// snapshot, Pending here conclusively permits a retry.
    pub(super) fn reconcile_submission(
        &mut self,
        session: &SessionReference,
        id: ApprovalId,
        snapshot: &SessionSnapshot,
    ) {
        if self
            .visible
            .as_ref()
            .is_some_and(|(owner, visible)| owner == session && *visible == id)
        {
            self.awaiting_confirmation = false;
            self.reconcile(session, snapshot);
        }
    }

    pub(super) fn command(&mut self, command: SemanticCommandId) -> Option<(ApprovalId, Decision)> {
        use SemanticCommandId::*;
        if self.submitting {
            return None;
        }
        match command {
            ApprovalChoicePrevious => {
                self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)));
                self.window.reveal();
            }
            ApprovalChoiceNext => {
                self.selected = Some(
                    self.selected
                        .map_or(0, |index| (index + 1).min(DECISIONS.len() - 1)),
                );
                self.window.reveal();
            }
            ApprovalAccept => return self.begin(Decision::Accept),
            ApprovalAcceptForSession => return self.begin(Decision::AcceptForSession),
            ApprovalDecline => return self.begin(Decision::Decline),
            ApprovalDeclineAndInterrupt => return self.begin(Decision::DeclineAndInterrupt),
            ApprovalChoose => return self.begin(DECISIONS[self.selected?].0),
            _ => {}
        }
        None
    }

    fn begin(&mut self, decision: Decision) -> Option<(ApprovalId, Decision)> {
        let id = self.id()?;
        self.submitting = true;
        self.awaiting_confirmation = true;
        Some((id, decision))
    }

    /// Draws the panel over the composer, answering the area it covers.
    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        composer: Rect,
        activity: &Activity,
        theme: &Theme,
    ) -> Rect {
        let Activity::Approval {
            approval,
            tool_activity_id,
            detail_truncated,
            outcome,
            ..
        } = activity
        else {
            return Rect::default();
        };
        let mut detail = detail_lines(approval, *tool_activity_id)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        if *detail_truncated {
            detail.push(Line::styled(
                "[Approval detail truncated]",
                theme.text.subdued,
            ));
        }
        let mut decisions = Vec::with_capacity(DECISIONS.len());
        for (index, (_, label)) in DECISIONS.iter().enumerate() {
            decisions.push(Line::styled(
                format!(
                    "{} {}. {label}",
                    if Some(index) == self.selected {
                        ">"
                    } else {
                        " "
                    },
                    index + 1
                ),
                if Some(index) == self.selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                },
            ));
        }
        let hint = Line::styled(
            "↑/↓ choose · Enter decide · 1–4 decide · Esc hide",
            theme.text.subdued,
        );
        let detail = Paragraph::new(detail)
            .style(theme.text.primary)
            .wrap(Wrap { trim: false });
        let detail_rows = detail.line_count(composer.width.saturating_sub(2));
        // Every Decision and the keys' hint beneath them.
        let wanted_controls = decisions.len() + 1;
        let height = (detail_rows
            .saturating_add(wanted_controls)
            .saturating_add(2)
            .min(u16::MAX as usize) as u16)
            .min(frame.area().height.saturating_sub(4))
            .min(24);
        let bottom = composer.bottom().min(frame.area().bottom());
        let area = Rect::new(
            composer.x,
            bottom.saturating_sub(height),
            composer.width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, area);
        let title = if self.submitting || *outcome == ApprovalOutcome::Submitting {
            " Approval · Submitting "
        } else if *outcome == ApprovalOutcome::SubmissionRejected {
            " Approval · Decision not delivered; retry "
        } else {
            " Approval · Choose Decision "
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // A panel too short for its controls gives up the keys' hint first,
        // then shows the Decisions through a window that keeps the one the
        // reader is on in view.
        let decision_rows = (decisions.len() as u16).min(inner.height);
        let hint_rows = u16::from(inner.height > decision_rows);
        let mut controls = self
            .window
            .show(decisions, usize::from(decision_rows), self.selected)
            .collect::<Vec<_>>();
        if hint_rows > 0 {
            controls.push(hint);
        }
        let controls_height = decision_rows + hint_rows;
        let detail_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(controls_height),
        );
        let controls_area = Rect::new(inner.x, detail_area.bottom(), inner.width, controls_height);
        frame.render_widget(detail, detail_area);
        frame.render_widget(Paragraph::new(controls), controls_area);
        area
    }
}

pub(super) fn pending(snapshot: &SessionSnapshot) -> impl Iterator<Item = &Activity> {
    snapshot.activities.iter().filter(|activity| {
        matches!(activity, Activity::Approval { approval, .. }
            if snapshot.pending_approvals.contains(&approval.id))
    })
}

pub(super) fn key(event: &Event) -> Option<CommandId> {
    use SemanticCommandId::*;
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => ApprovalHide,
        (KeyCode::Up, KeyModifiers::NONE) => ApprovalChoicePrevious,
        (KeyCode::Down, KeyModifiers::NONE) => ApprovalChoiceNext,
        (KeyCode::Enter, KeyModifiers::NONE) => ApprovalChoose,
        (KeyCode::Char('1'), KeyModifiers::NONE) => ApprovalAccept,
        (KeyCode::Char('2'), KeyModifiers::NONE) => ApprovalAcceptForSession,
        (KeyCode::Char('3'), KeyModifiers::NONE) => ApprovalDecline,
        (KeyCode::Char('4'), KeyModifiers::NONE) => ApprovalDeclineAndInterrupt,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

pub(super) fn decision_text(decision: Decision) -> &'static str {
    match decision {
        Decision::Accept => "Accepted once",
        Decision::AcceptForSession => "Accepted for Session",
        Decision::Decline => "Declined",
        Decision::DeclineAndInterrupt => "Declined and interrupted",
    }
}

pub(super) fn outcome_text(
    outcome: ApprovalOutcome,
    decision: Option<Decision>,
    follow_up_error: Option<&str>,
) -> String {
    if outcome == ApprovalOutcome::Decided
        && let Some(error) = follow_up_error
    {
        return match decision {
            Some(Decision::DeclineAndInterrupt) => {
                format!("Declined; interruption failed: {error}")
            }
            Some(decision) => format!("{}; follow-up failed: {error}", decision_text(decision)),
            None => format!("Decision delivered; follow-up failed: {error}"),
        };
    }
    match (outcome, decision) {
        (ApprovalOutcome::Pending, _) => "Pending".into(),
        (ApprovalOutcome::Submitting, _) => "Submitting".into(),
        (ApprovalOutcome::SubmissionRejected, _) => "Decision not delivered; retry".into(),
        (ApprovalOutcome::Decided, Some(decision)) => decision_text(decision).into(),
        (ApprovalOutcome::Decided, None) => "Decided".into(),
        (ApprovalOutcome::Withdrawn, _) => {
            "Withdrawn; Provider no longer requests this action".into()
        }
        (ApprovalOutcome::TurnEnded, _) => "Turn ended before a Decision".into(),
        (ApprovalOutcome::Unavailable, _) => {
            "Unavailable; previous Provider request is no longer live".into()
        }
        (ApprovalOutcome::DeliveryUncertain, _) => {
            "Delivery uncertain; Decision will not be resent".into()
        }
    }
}

pub(super) fn subject_summary(subject: &ApprovalSubject) -> String {
    match subject {
        ApprovalSubject::Command { command, .. } => compact_subject("Command", Some(command)),
        ApprovalSubject::FileChange { paths, .. } => paths.first().map_or_else(
            || compact_subject("File Change", None),
            |path| compact_subject("File Change", Some(&path.to_string_lossy())),
        ),
        ApprovalSubject::Read { path } => compact_subject("Read", Some(&path.to_string_lossy())),
        ApprovalSubject::Network { host_or_url } => compact_subject("Network", Some(host_or_url)),
        ApprovalSubject::PermissionGrant { .. } => compact_subject("Permission Grant", None),
        ApprovalSubject::OtherTool { name, .. } => compact_subject("Other Tool", Some(name)),
    }
}

/// A Fold header identifies the subject without repeating arbitrary Provider
/// input. Everything omitted here remains available in the expanded detail.
fn compact_subject(kind: &str, detail: Option<&str>) -> String {
    const MAX_CHARS: usize = 40;
    let Some(detail) = detail else {
        return kind.into();
    };
    let prefix = format!("{kind} · ");
    let first_line = detail.split('\n').next().unwrap_or_default();
    let available = MAX_CHARS
        .saturating_sub(prefix.chars().count())
        .saturating_sub(1);
    let mut kept = first_line.chars().take(available).collect::<String>();
    if detail.contains('\n') || first_line.chars().count() > available {
        kept.push('…');
    }
    format!("{prefix}{kept}")
}

pub(super) fn detail_lines(
    approval: &Approval,
    tool_activity_id: Option<crate::protocol::ActivityId>,
) -> Vec<String> {
    let mut lines = Vec::new();
    match &approval.subject {
        ApprovalSubject::Command {
            command,
            cwd,
            actions,
        } => {
            lines.push(format!("Command: {command}"));
            if let Some(cwd) = cwd {
                lines.push(format!("Directory: {}", cwd.display()));
            }
            for action in actions {
                match action {
                    CommandAction::Read {
                        command,
                        name,
                        path,
                    } => {
                        lines.push(format!("Read action: {command}"));
                        lines.push(format!("Name: {name}"));
                        lines.push(format!("Path: {}", path.display()));
                    }
                    CommandAction::ListFiles { command, path } => {
                        lines.push(format!("List files: {command}"));
                        if let Some(path) = path {
                            lines.push(format!("Path: {}", path.display()));
                        }
                    }
                    CommandAction::Search {
                        command,
                        query,
                        path,
                    } => {
                        lines.push(format!("Search: {command}"));
                        if let Some(query) = query {
                            lines.push(format!("Query: {query}"));
                        }
                        if let Some(path) = path {
                            lines.push(format!("Path: {}", path.display()));
                        }
                    }
                    CommandAction::Unknown { command } => {
                        lines.push(format!("Action: {command}"));
                    }
                }
            }
        }
        ApprovalSubject::FileChange { paths, grant_root } => {
            for (index, path) in paths.iter().enumerate() {
                lines.push(format!(
                    "{}: {}",
                    if index == 0 { "File Change" } else { "Path" },
                    path.display()
                ));
            }
            if let Some(root) = grant_root {
                lines.push(format!("Grant root: {}", root.display()));
            }
        }
        ApprovalSubject::Read { path } => lines.push(format!("Read: {}", path.display())),
        ApprovalSubject::Network { host_or_url } => {
            lines.push(format!("Network: {host_or_url}"));
        }
        ApprovalSubject::PermissionGrant { profile } => {
            push_json(&mut lines, "Permission Grant", profile);
        }
        ApprovalSubject::OtherTool { name, input } => {
            lines.push(format!("Other Tool: {name}"));
            push_json(&mut lines, "Input", input);
        }
    }
    if tool_activity_id.is_some() {
        let name = match &approval.subject {
            ApprovalSubject::Command { .. } => "Command",
            ApprovalSubject::FileChange { .. } => "File Change",
            _ => "Tool",
        };
        lines.push(format!("Tool row: {name}"));
    }
    if let Some(reason) = &approval.reason {
        lines.push(format!("Reason: {reason}"));
    }
    let mut logical_lines = Vec::new();
    for line in lines {
        logical_lines.extend(line.split('\n').map(str::to_owned));
    }
    logical_lines
}

fn push_json(lines: &mut Vec<String>, label: &str, value: &serde_json::Value) {
    let rendered = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut parts = rendered.lines();
    lines.push(format!("{label}: {}", parts.next().unwrap_or_default()));
    lines.extend(parts.map(|line| format!("  {line}")));
}
