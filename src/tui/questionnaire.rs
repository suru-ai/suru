//! Client-local Answer editing; no Provider work or composer draft lives here.
use super::{commands::SemanticCommandId, state::CommandId};
use crate::{
    protocol::{
        Activity, Answer, Question, QuestionAnswer, Questionnaire, QuestionnaireId,
        QuestionnaireOutcome, SessionReference, SessionSnapshot,
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
    questions: Vec<QuestionDraft>,
    current: usize,
    review: bool,
    submitting: bool,
    scroll: usize,
}

#[derive(Clone, Default)]
struct QuestionDraft {
    cursor: usize,
    choices: Vec<String>,
    text: String,
    editing_text: bool,
    error: Option<String>,
}

impl std::fmt::Debug for QuestionDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QuestionDraft(<redacted>)")
    }
}

impl QuestionDraft {
    fn answer(&self) -> QuestionAnswer {
        match (self.choices.is_empty(), self.text.is_empty()) {
            (false, false) => QuestionAnswer::SelectedWithFreeform {
                choices: self.choices.clone(),
                text: self.text.clone(),
            },
            (false, true) => QuestionAnswer::Selected {
                choices: self.choices.clone(),
            },
            (true, false) => QuestionAnswer::Freeform {
                text: self.text.clone(),
            },
            (true, true) => QuestionAnswer::Omitted,
        }
    }
}

impl Panel {
    fn answer(&self) -> Answer {
        Answer {
            questions: self.questions.iter().map(QuestionDraft::answer).collect(),
        }
    }
    fn validate_question(&mut self, questionnaire: &Questionnaire, index: usize) -> bool {
        if questionnaire.questions[index].accepts(&self.questions[index].answer()) {
            self.questions[index].error = None;
            true
        } else {
            self.current = index;
            self.review = false;
            self.scroll = 0;
            self.questions[index].error = Some(format!(
                "Question {} requires a supported answer",
                index + 1
            ));
            false
        }
    }
    fn review(&mut self, questionnaire: &Questionnaire) {
        for index in 0..self.questions.len() {
            if !self.validate_question(questionnaire, index) {
                return;
            }
        }
        self.review = true;
        self.scroll = 0;
    }
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
        self.drafts.entry(key.clone()).or_insert_with(|| Panel {
            questions: vec![QuestionDraft::default(); questionnaire.questions.len()],
            ..Panel::default()
        });
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
        let question = &questionnaire.questions[panel.current];
        if panel.review || panel.submitting || !question.freeform {
            return;
        }
        let draft = &mut panel.questions[panel.current];
        if !question.combine_freeform {
            draft.choices.clear();
        }
        draft.text.push_str(text);
        draft.error = None;
        draft.editing_text = true;
    }
    pub(super) fn delete(&mut self) {
        if let Some(panel) = self
            .visible
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))
        {
            if !panel.review && !panel.submitting {
                panel.questions[panel.current].text.pop();
            }
        }
    }
    pub(super) fn command(
        &mut self,
        questionnaire: &Questionnaire,
        command: SemanticCommandId,
    ) -> Option<Answer> {
        use SemanticCommandId::*;
        let panel = self
            .visible
            .as_ref()
            .and_then(|key| self.drafts.get_mut(key))?;
        if panel.submitting {
            return None;
        }
        let question = &questionnaire.questions[panel.current];
        match command {
            QuestionnaireScrollUp => panel.scroll = panel.scroll.saturating_sub(3),
            QuestionnaireScrollDown => panel.scroll = panel.scroll.saturating_add(3),
            QuestionnaireBack => {
                if !panel.review {
                    panel.current = panel.current.saturating_sub(1);
                }
                panel.review = false;
                panel.scroll = 0;
            }
            QuestionnaireNext => {
                if !panel.validate_question(questionnaire, panel.current) {
                    return None;
                }
                if panel.current + 1 < panel.questions.len() {
                    panel.current += 1;
                    panel.scroll = 0;
                    panel.review = false;
                } else {
                    panel.review(questionnaire);
                }
            }
            QuestionnaireChoicePrevious | QuestionnaireChoiceNext => {
                panel.review = false;
                let draft = &mut panel.questions[panel.current];
                draft.editing_text = false;
                draft.cursor = if command == QuestionnaireChoicePrevious {
                    draft.cursor.saturating_sub(1)
                } else {
                    (draft.cursor + 1).min(question.choices.len().saturating_sub(1))
                };
            }
            QuestionnaireSelect => {
                let draft = &mut panel.questions[panel.current];
                if draft.editing_text && !panel.review {
                    draft.text.push(' ');
                } else if let Some(choice) = question.choices.get(draft.cursor) {
                    panel.review = false;
                    if !question.combine_freeform {
                        draft.text.clear();
                    }
                    if let Some(index) = draft.choices.iter().position(|id| id == &choice.id) {
                        draft.choices.remove(index);
                    } else {
                        if !question.multiple {
                            draft.choices.clear();
                        }
                        draft.choices.push(choice.id.clone());
                    }
                    draft.error = None;
                }
            }
            QuestionnaireOmit if !question.required => {
                panel.questions[panel.current] = QuestionDraft::default();
                panel.review = false;
            }
            QuestionnaireReview => panel.review(questionnaire),
            QuestionnaireSubmit if panel.review => {
                let answer = panel.answer();
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
        let question = &questionnaire.questions[panel.current];
        let draft = &panel.questions[panel.current];
        let mut lines = Vec::new();
        let navigation;
        if panel.review {
            lines.push(Line::styled("Review Answer", theme.accent.primary));
            for (index, (question, draft)) in questionnaire
                .questions
                .iter()
                .zip(&panel.questions)
                .enumerate()
            {
                lines.push(Line::from(format!("{}. {}", index + 1, question.text)));
                lines.push(Line::from(answer_text(question, Some(&draft.answer()))));
            }
            navigation = if panel.submitting {
                "Submitting…"
            } else {
                "Ctrl+Enter submit · Shift+Tab back · Esc hide"
            }
            .to_owned();
        } else {
            lines.push(Line::styled(
                format!(
                    "Question {} of {}",
                    panel.current + 1,
                    questionnaire.questions.len()
                ),
                theme.accent.primary,
            ));
            if let Some(title) = &question.title {
                lines.push(Line::styled(title.clone(), theme.text.subdued));
            }
            if let Some(error) = &draft.error {
                lines.push(Line::styled(error.clone(), theme.feedback.error));
            }
            lines.push(Line::from(question.text.clone()));
            for (index, choice) in question.choices.iter().enumerate() {
                let selected = draft.choices.contains(&choice.id);
                let recommendation = if choice.recommended
                    && !choice.label.to_ascii_lowercase().ends_with("(recommended)")
                {
                    " (recommended)"
                } else {
                    ""
                };
                lines.push(Line::styled(
                    format!(
                        "{} {}{}",
                        if selected { "[x]" } else { "[ ]" },
                        choice.label,
                        recommendation
                    ),
                    if index == draft.cursor {
                        theme.selection.focused
                    } else if choice.recommended {
                        theme.accent.primary
                    } else {
                        theme.text.primary
                    },
                ));
                if let Some(description) = &choice.description {
                    lines.push(Line::styled(
                        format!("    {description}"),
                        theme.text.subdued,
                    ));
                }
            }
            if question.freeform {
                let value = if question.secret && !draft.text.is_empty() {
                    "••••••••"
                } else {
                    &draft.text
                };
                lines.push(Line::from(format!(
                    "{}: {value}",
                    if question.combine_freeform {
                        "Additional text"
                    } else {
                        "Text"
                    }
                )));
            }
            navigation = format!(
                "↑/↓ choose · Space select · Enter {} · Shift+Tab back{}",
                if panel.current + 1 < questionnaire.questions.len() {
                    "next"
                } else {
                    "review"
                },
                if !question.required {
                    " · Ctrl+O omit"
                } else {
                    ""
                }
            );
        }
        let footer = vec![
            Line::from(navigation),
            Line::from("Ctrl+D decline · Esc hide · then Esc Esc to interrupt"),
        ];
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

pub(super) fn answer_text(question: &Question, answer: Option<&QuestionAnswer>) -> String {
    let labels = |choices: &[String]| {
        choices
            .iter()
            .map(|id| {
                question
                    .choices
                    .iter()
                    .find(|choice| &choice.id == id)
                    .map_or(id.as_str(), |choice| choice.label.as_str())
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match answer {
        Some(QuestionAnswer::SecretAnswered) => "Answered (secret)".into(),
        Some(QuestionAnswer::Omitted) | None => "Not answered".into(),
        Some(_) if question.secret => "••••••••".into(),
        Some(QuestionAnswer::Selected { choices }) => labels(choices),
        Some(QuestionAnswer::SelectedWithFreeform { choices, text }) => {
            format!("{}; {}", labels(choices), text)
        }
        Some(QuestionAnswer::Freeform { text }) => text.clone(),
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
        (KeyCode::Enter, _) | (KeyCode::Tab, _) => QuestionnaireNext,
        (KeyCode::BackTab, _) => QuestionnaireBack,
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => QuestionnaireOmit,
        (KeyCode::Up, _) => QuestionnaireChoicePrevious,
        (KeyCode::Down, _) => QuestionnaireChoiceNext,
        (KeyCode::Char(' '), KeyModifiers::NONE) => QuestionnaireSelect,
        (KeyCode::Backspace, _) => return Some(CommandId::QuestionnaireDelete),
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            return Some(CommandId::QuestionnaireInsert(c.to_string()));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}
