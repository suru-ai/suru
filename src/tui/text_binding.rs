//! Typed things bound to spans of a Prompt's text.
//!
//! A [`TextBinding`] pins one kind of thing to the byte span of the text that
//! stands for it; a Skill Invocation, written as `$skill-name`, is the only
//! kind so far. The span bookkeeping every kind shares lives in
//! [`TextBindings`]: bindings stay in text order, a span moves with an edit
//! made wholly before it, an edit reaching into a span drops its binding, and
//! a Prompt's bindings are raised from, and lowered back into, the lists the
//! protocol carries beside its text. A kind supplies only what is its own: how
//! the text at its span is recognized, and how that span is drawn.

mod skill;

use std::ops::Range;

use ratatui::style::Style;

use crate::{
    protocol::{InitialPrompt, Message, PromptId, SkillInvocation},
    theme::Theme,
};

pub(super) use skill::{SkillIssue, skill_invocation_can_start};

/// One typed thing bound to the span of text that stands for it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TextBinding {
    pub(super) span: Range<usize>,
    pub(super) kind: BindingKind,
}

/// What a span of text is bound to.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum BindingKind {
    /// A Skill Invocation, standing in the text as `$skill-name`.
    Skill(skill::BoundSkill),
}

impl BindingKind {
    /// Whether `text`, found at a binding's span, still stands for what the
    /// span is bound to.
    fn is_written_as(&self, text: &str) -> bool {
        match self {
            Self::Skill(skill) => skill.is_written_as(text),
        }
    }

    /// The style bound text is drawn in over the plain text around it.
    pub(super) fn style(&self, theme: &Theme) -> Style {
        match self {
            Self::Skill(_) => theme.accent.primary,
        }
    }
}

impl TextBinding {
    /// Whether the span still lies within `text` and reads as its kind there.
    fn is_recognized_in(&self, text: &str) -> bool {
        text.get(self.span.clone())
            .is_some_and(|bound| self.kind.is_written_as(bound))
    }

    fn skill(&self) -> Option<(&Range<usize>, &skill::BoundSkill)> {
        match &self.kind {
            BindingKind::Skill(skill) => Some((&self.span, skill)),
        }
    }

    fn skill_mut(&mut self) -> Option<(&Range<usize>, &mut skill::BoundSkill)> {
        match &mut self.kind {
            BindingKind::Skill(skill) => Some((&self.span, skill)),
        }
    }
}

/// The bindings beside one text, in text order.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct TextBindings(Vec<TextBinding>);

impl TextBindings {
    /// The bindings a Prompt carries beside its text.
    pub(super) fn from_prompt(prompt: &InitialPrompt) -> Self {
        Self::from_lists(&prompt.skill_invocations)
    }

    /// The bindings a Message carries beside its content.
    pub(super) fn from_message(message: &Message) -> Self {
        Self::from_lists(&message.skill_invocations)
    }

    fn from_lists(skill_invocations: &[SkillInvocation]) -> Self {
        let mut bindings = skill_invocations
            .iter()
            .map(skill::BoundSkill::binding)
            .collect::<Vec<_>>();
        bindings.sort_by_key(|binding| binding.span.start);
        Self(bindings)
    }

    /// A Prompt of `text` carrying these bindings beside it, each kind in the
    /// list the protocol carries it in.
    pub(super) fn into_prompt(self, id: PromptId, text: String) -> InitialPrompt {
        InitialPrompt {
            id,
            text,
            skill_invocations: self.skill_invocations(),
            attachments: Vec::new(),
        }
    }

    /// Whether these are exactly the bindings `prompt` carries beside its text.
    pub(super) fn are_carried_by(&self, prompt: &InitialPrompt) -> bool {
        self.skill_invocations() == prompt.skill_invocations
    }

    fn skill_invocations(&self) -> Vec<SkillInvocation> {
        self.skills()
            .map(|(span, skill)| skill.invocation(span))
            .collect()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &TextBinding> {
        self.0.iter()
    }

    /// The bindings whose spans still read as their kinds in `text`.
    pub(super) fn recognized_in<'a>(
        &'a self,
        text: &'a str,
    ) -> impl Iterator<Item = &'a TextBinding> {
        self.0
            .iter()
            .filter(|binding| binding.is_recognized_in(text))
    }

    /// Drops every binding whose span no longer reads as its kind in `text`.
    pub(super) fn retain_recognized(&mut self, text: &str) {
        self.0.retain(|binding| binding.is_recognized_in(text));
    }

    /// Follows an edit that replaced the `edited` bytes with
    /// `replacement_len` others. A span wholly after the edit moves by the
    /// change in length, one wholly before it stays where it is, and one the
    /// edit reaches into is dropped along with its binding.
    pub(super) fn follow_edit(&mut self, edited: Range<usize>, replacement_len: usize) {
        let removed_len = edited.end.saturating_sub(edited.start);
        let delta = replacement_len as isize - removed_len as isize;
        self.0.retain_mut(|binding| {
            let span = &mut binding.span;
            if edited.end <= span.start {
                span.start = shift(span.start, delta);
                span.end = shift(span.end, delta);
                true
            } else {
                edited.start >= span.end
            }
        });
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Binds `kind` to `span`, keeping the bindings in text order.
    fn bind(&mut self, span: Range<usize>, kind: BindingKind) {
        self.0.push(TextBinding { span, kind });
        self.0.sort_by_key(|binding| binding.span.start);
    }

    /// Whether any binding's span overlaps `range`.
    fn overlaps(&self, range: &Range<usize>) -> bool {
        self.0
            .iter()
            .any(|binding| binding.span.start < range.end && range.start < binding.span.end)
    }

    fn skills(&self) -> impl Iterator<Item = (&Range<usize>, &skill::BoundSkill)> {
        self.0.iter().filter_map(TextBinding::skill)
    }

    fn skills_mut(&mut self) -> impl Iterator<Item = (&Range<usize>, &mut skill::BoundSkill)> {
        self.0.iter_mut().filter_map(TextBinding::skill_mut)
    }

    /// Keeps every binding of another kind, and each Skill Invocation `keep`
    /// accepts.
    fn retain_skills(&mut self, mut keep: impl FnMut(&skill::BoundSkill) -> bool) {
        self.0
            .retain(|binding| binding.skill().is_none_or(|(_, skill)| keep(skill)));
    }
}

fn shift(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SkillId, TextSpan};

    fn invocation(name: &str, span: Range<usize>) -> SkillInvocation {
        SkillInvocation {
            skill_id: SkillId::new(format!("{name}-id")),
            name: name.to_owned(),
            scope: None,
            span: TextSpan::from(span),
        }
    }

    fn prompt(text: &str, skill_invocations: Vec<SkillInvocation>) -> InitialPrompt {
        InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations,
            attachments: Vec::new(),
        }
    }

    fn spans(bindings: &TextBindings) -> Vec<Range<usize>> {
        bindings
            .iter()
            .map(|binding| binding.span.clone())
            .collect()
    }

    #[test]
    fn a_span_moves_with_edits_wholly_before_it_and_stays_for_edits_after_it() {
        // "Use $review now"
        let mut bindings = TextBindings::from_prompt(&prompt(
            "Use $review now",
            vec![invocation("review", 4..11)],
        ));

        bindings.follow_edit(0..0, 1);
        assert_eq!(spans(&bindings), vec![5..12]);
        bindings.follow_edit(0..4, 0);
        assert_eq!(spans(&bindings), vec![1..8]);
        bindings.follow_edit(8..8, 3);
        assert_eq!(spans(&bindings), vec![1..8]);
        bindings.follow_edit(8..11, 0);
        assert_eq!(spans(&bindings), vec![1..8]);
        bindings.follow_edit(1..1, 2);
        assert_eq!(spans(&bindings), vec![3..10]);
    }

    #[test]
    fn an_edit_reaching_into_a_span_drops_only_that_binding() {
        let text = "$review $review";
        let inside = |edited: Range<usize>, replacement_len: usize| {
            let mut bindings = TextBindings::from_prompt(&prompt(
                text,
                vec![invocation("review", 0..7), invocation("review", 8..15)],
            ));
            bindings.follow_edit(edited, replacement_len);
            spans(&bindings)
        };

        assert_eq!(inside(3..3, 1), vec![9..16]);
        assert_eq!(inside(0..3, 0), vec![5..12]);
        assert_eq!(inside(6..8, 0), vec![6..13]);
        assert_eq!(inside(0..7, 7), vec![8..15]);
        assert_eq!(inside(7..9, 0), vec![0..7]);
        assert_eq!(inside(0..15, 0), Vec::<Range<usize>>::new());
    }

    #[test]
    fn bindings_whose_text_no_longer_reads_as_their_kind_are_not_recognized() {
        let mut bindings = TextBindings::from_prompt(&prompt(
            "$review $lint",
            vec![invocation("review", 0..7), invocation("lint", 8..13)],
        ));

        let recognized = bindings
            .recognized_in("$REVIEW $lent")
            .map(|binding| binding.span.clone())
            .collect::<Vec<_>>();
        assert_eq!(recognized, vec![0..7]);
        assert_eq!(bindings.recognized_in("$review").count(), 1);

        bindings.retain_recognized("$review $lent");
        assert_eq!(spans(&bindings), vec![0..7]);
    }

    #[test]
    fn a_prompt_raised_into_bindings_lowers_back_to_the_prompt_in_text_order() {
        let original = prompt(
            "$review then $lint",
            vec![invocation("lint", 13..18), invocation("review", 0..7)],
        );
        let bindings = TextBindings::from_prompt(&original);
        assert_eq!(spans(&bindings), vec![0..7, 13..18]);

        let lowered = bindings.into_prompt(original.id, original.text.clone());
        assert_eq!(
            lowered.skill_invocations,
            vec![invocation("review", 0..7), invocation("lint", 13..18)]
        );
        assert!(TextBindings::from_prompt(&lowered).are_carried_by(&lowered));
        assert!(!TextBindings::default().are_carried_by(&lowered));
    }
}
