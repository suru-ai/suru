//! Client-local Answer editing; no Provider work or composer draft lives here.
use super::{commands::SemanticCommandId, state::CommandId};
use crate::{
    protocol::{
        Activity, Answer, QuestionAnswer, Questionnaire, QuestionnaireId, QuestionnaireOutcome,
        SessionReference, SessionSnapshot,
    },
    theme::Theme,
};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Default)]
pub(super) struct QuestionnairePanels {
    drafts: HashMap<(SessionReference, QuestionnaireId), Panel>,
    availability:
        HashMap<SessionReference, (crate::protocol::SessionRevision, Vec<QuestionnaireId>)>,
    unlisted_sessions: HashSet<SessionReference>,
    pub(super) visible: Option<(SessionReference, QuestionnaireId)>,
}
#[derive(Clone, Debug, Default)]
struct Panel {
    cursor: usize,
    answer: Option<QuestionAnswer>,
    review: bool,
    error: Option<String>,
    submitting: bool,
    editing_text: bool,
    scroll: usize,
}

pub(super) fn pending(snapshot: &SessionSnapshot) -> impl Iterator<Item = &Questionnaire> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Questionnaire {
                questionnaire,
                outcome: QuestionnaireOutcome::Pending,
                turn_id,
                ..
            } if snapshot.turns.iter().any(|turn| {
                turn.id == *turn_id && turn.status == crate::protocol::TurnStatus::Active
            }) =>
            {
                Some(questionnaire)
            }
            _ => None,
        })
}

impl QuestionnairePanels {
    pub(super) fn discard_origin(&mut self, origin: &crate::protocol::Outlook) {
        let sessions: Vec<_> = self
            .availability
            .keys()
            .filter(|session| &session.origin == origin)
            .cloned()
            .collect();
        for session in sessions {
            self.discard_session(&session);
        }
    }

    pub(super) fn retain_origin(
        &mut self,
        origin: &crate::protocol::Outlook,
        sessions: &[crate::protocol::SessionId],
    ) {
        let removed: Vec<_> = self
            .availability
            .keys()
            .filter(|session| {
                &session.origin == origin
                    && !self.unlisted_sessions.contains(*session)
                    && !sessions.contains(&session.session_id)
            })
            .cloned()
            .collect();
        for session in removed {
            self.discard_session(&session);
        }
    }
    pub(super) fn discard_session(&mut self, session: &SessionReference) {
        self.availability.remove(session);
        self.unlisted_sessions.remove(session);
        self.drafts.retain(|(owner, _), _| owner != session);
        if self
            .visible
            .as_ref()
            .is_some_and(|(owner, _)| owner == session)
        {
            self.visible = None;
        }
    }

    pub(super) fn reconcile(&mut self, session: &SessionReference, snapshot: &SessionSnapshot) {
        if snapshot.session.parent.is_some() {
            self.unlisted_sessions.insert(session.clone());
        }

        self.reconcile_available(
            session,
            snapshot.revision,
            &pending(snapshot).map(|q| q.id).collect::<Vec<_>>(),
        );
    }

    pub(super) fn reconcile_available(
        &mut self,
        session: &SessionReference,
        revision: crate::protocol::SessionRevision,
        available: &[QuestionnaireId],
    ) {
        if self
            .availability
            .get(session)
            .is_some_and(|(known, _)| known.0 > revision.0)
        {
            return;
        }
        self.availability
            .insert(session.clone(), (revision, available.to_vec()));
        self.drafts
            .retain(|(owner, id), _| owner != session || available.contains(id));
        if self
            .visible
            .as_ref()
            .is_some_and(|(owner, id)| owner == session && !available.contains(id))
        {
            self.visible = None;
        }
    }
    pub(super) fn is_open(&self, session: Option<&SessionReference>) -> bool {
        self.visible
            .as_ref()
            .is_some_and(|(owner, _)| Some(owner) == session)
    }
    pub(super) fn available(&self, session: &SessionReference, id: QuestionnaireId) -> bool {
        self.availability
            .get(session)
            .is_none_or(|(_, ids)| ids.contains(&id))
    }
    pub(super) fn open(&mut self, session: SessionReference, questionnaire: &Questionnaire) {
        if !self.available(&session, questionnaire.id) {
            return;
        }
        let key = (session, questionnaire.id);
        self.drafts.entry(key.clone()).or_default();
        self.visible = Some(key);
    }
    pub(super) fn hide(&mut self) {
        self.visible = None;
    }
    pub(super) fn id(&self) -> Option<QuestionnaireId> {
        self.visible.as_ref().map(|(_, id)| *id)
    }
    pub(super) fn insert(&mut self, questionnaire: &Questionnaire, text: &str) {
        let Some(panel) = self
            .visible
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
        else {
            return;
        };
        if panel.review || panel.submitting || !questionnaire.questions[0].freeform {
            return;
        }
        let answer = panel
            .answer
            .get_or_insert_with(|| QuestionAnswer::Freeform {
                text: String::new(),
            });
        if !matches!(answer, QuestionAnswer::Freeform { .. }) {
            *answer = QuestionAnswer::Freeform {
                text: String::new(),
            };
        }
        if let QuestionAnswer::Freeform { text: value } = answer {
            value.push_str(text);
        }
        panel.error = None;
        panel.editing_text = true;
    }
    pub(super) fn delete(&mut self) {
        if let Some(panel) = self
            .visible
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
        {
            if panel.review || panel.submitting {
                return;
            }
            if let Some(QuestionAnswer::Freeform { text }) = &mut panel.answer {
                text.pop();
            }
        }
    }
    pub(super) fn command(
        &mut self,
        questionnaire: &Questionnaire,
        command: SemanticCommandId,
    ) -> Option<Answer> {
        let panel = self
            .visible
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))?;
        if panel.submitting {
            return None;
        }
        let question = &questionnaire.questions[0];
        match command {
            SemanticCommandId::QuestionnaireScrollUp => {
                panel.scroll = panel.scroll.saturating_sub(3)
            }
            SemanticCommandId::QuestionnaireScrollDown => {
                panel.scroll = panel.scroll.saturating_add(3)
            }
            SemanticCommandId::QuestionnairePrevious => {
                panel.review = false;
                panel.cursor = panel.cursor.saturating_sub(1);
                panel.editing_text = false;
            }
            SemanticCommandId::QuestionnaireNext => {
                panel.review = false;
                panel.editing_text = false;
                panel.cursor = (panel.cursor + 1).min(question.choices.len().saturating_sub(1));
            }
            SemanticCommandId::QuestionnaireSelect if panel.editing_text && !panel.review => {
                if let Some(QuestionAnswer::Freeform { text }) = &mut panel.answer {
                    text.push(' ');
                }
            }
            SemanticCommandId::QuestionnaireSelect => {
                panel.review = false;
                if let Some(choice) = question.choices.get(panel.cursor) {
                    panel.answer = Some(QuestionAnswer::Selected {
                        choices: vec![choice.id.clone()],
                    });
                    panel.error = None;
                }
            }
            SemanticCommandId::QuestionnaireReview => {
                let answer = Answer {
                    questions: vec![panel.answer.clone().unwrap_or(QuestionAnswer::Omitted)],
                };
                match questionnaire.validate(&answer) {
                    Ok(()) => {
                        panel.review = true;
                        panel.error = None;
                    }
                    Err(error) => panel.error = Some(error),
                }
            }
            SemanticCommandId::QuestionnaireSubmit if panel.review => {
                let answer = Answer {
                    questions: vec![panel.answer.clone()?],
                };
                if questionnaire.validate(&answer).is_ok() {
                    panel.submitting = true;
                    return Some(answer);
                }
            }
            _ => {}
        }
        None
    }
    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        composer: Rect,
        questionnaire: &Questionnaire,
        theme: &Theme,
        position: (usize, usize),
    ) {
        let Some(panel) = self.visible.as_ref().and_then(|key| self.drafts.get(key)) else {
            return;
        };
        let question = &questionnaire.questions[0];
        let mut lines = vec![
            Line::styled(
                if panel.review {
                    "Review Answer"
                } else {
                    "Question 1 of 1"
                },
                theme.accent.primary,
            ),
            Line::from(question.text.clone()),
        ];
        if panel.review {
            lines.push(Line::from(answer_text(
                questionnaire,
                panel.answer.as_ref(),
            )));
            lines.push(Line::from(if panel.submitting {
                "Submitting…"
            } else {
                "Ctrl+Enter submit · Up edit · Esc hide"
            }));
        } else {
            for (index, choice) in question.choices.iter().enumerate() {
                let selected = matches!(&panel.answer, Some(QuestionAnswer::Selected { choices }) if choices.contains(&choice.id));
                lines.push(Line::styled(
                    format!(
                        "{} {}{}",
                        if selected { "[x]" } else { "[ ]" },
                        choice.label,
                        if choice.recommended {
                            " (recommended)"
                        } else {
                            ""
                        }
                    ),
                    if index == panel.cursor {
                        theme.selection.focused
                    } else if choice.recommended {
                        theme.accent.primary
                    } else {
                        theme.text.primary
                    },
                ));
            }
            if question.freeform {
                lines.push(Line::from(format!(
                    "Text: {}",
                    answer_text(questionnaire, panel.answer.as_ref())
                        .trim_start_matches("Not answered")
                )));
            }
            if let Some(error) = &panel.error {
                lines.push(Line::styled(error.clone(), theme.feedback.error));
            }
            lines.push(Line::from(
                "↑/↓ choose · Space select · Enter review · Esc hide",
            ));
        }
        lines.push(Line::from(
            "Ctrl+D decline · Hide panel, then Esc Esc to interrupt",
        ));
        let footer = lines.split_off(lines.len().saturating_sub(2));
        let paragraph = Paragraph::new(lines)
            .style(theme.text.primary)
            .wrap(Wrap { trim: false });
        let content_rows = paragraph.line_count(composer.width.saturating_sub(2));
        let height = (content_rows.saturating_add(5).min(u16::MAX as usize) as u16)
            .min(frame.area().height.saturating_sub(4))
            .min(20);
        let footer_rows = footer.len() as u16 + 1;
        let bottom = composer.bottom().min(frame.area().bottom());
        let area = Rect::new(
            composer.x,
            bottom.saturating_sub(height),
            composer.width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, area);
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Questionnaire {} of {} · Alt+←/→ switch ",
            position.0, position.1
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let content = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(footer_rows),
        );
        let scroll = panel
            .scroll
            .min(content_rows.saturating_sub(content.height as usize));
        frame.render_widget(paragraph.scroll((scroll as u16, 0)), content);
        let footer_area = Rect::new(
            inner.x,
            content.bottom(),
            inner.width,
            footer_rows.min(inner.height),
        );
        let mut footer = footer;
        footer.push(Line::styled(
            "Alt+↑/↓ scroll question · PgUp/PgDn browse Transcript",
            theme.text.subdued,
        ));
        frame.render_widget(
            Paragraph::new(footer).style(theme.text.primary),
            footer_area,
        );
    }
}

pub(super) fn answer_text(
    questionnaire: &Questionnaire,
    answer: Option<&QuestionAnswer>,
) -> String {
    match answer {
        Some(QuestionAnswer::SecretAnswered) => "Answered (secret)".into(),
        Some(_) if questionnaire.questions[0].secret => "••••••••".into(),
        Some(QuestionAnswer::Selected { choices }) => choices.join(", "),
        Some(QuestionAnswer::Freeform { text }) => text.clone(),
        _ => "Not answered".into(),
    }
}

pub(super) fn key(event: &Event) -> Option<CommandId> {
    use SemanticCommandId::*;
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != crossterm::event::KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Left, KeyModifiers::ALT) => QuestionnaireRequestPrevious,
        (KeyCode::Right, KeyModifiers::ALT) => QuestionnaireRequestNext,
        (KeyCode::Up, KeyModifiers::ALT) => QuestionnaireScrollUp,
        (KeyCode::Down, KeyModifiers::ALT) => QuestionnaireScrollDown,
        (KeyCode::Esc, _) => QuestionnaireHide,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => QuestionnaireDecline,
        (KeyCode::Enter, KeyModifiers::CONTROL) => QuestionnaireSubmit,
        (KeyCode::Enter, _) => QuestionnaireReview,
        (KeyCode::Up, _) => QuestionnairePrevious,
        (KeyCode::Down, _) => QuestionnaireNext,
        (KeyCode::Char(' '), KeyModifiers::NONE) => QuestionnaireSelect,
        (KeyCode::Backspace, _) => return Some(CommandId::QuestionnaireDelete),
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            return Some(CommandId::QuestionnaireInsert(c.to_string()));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}
