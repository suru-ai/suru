//! Typed things bound to spans of a Prompt's text.
//!
//! A [`TextBinding`] pins one kind of thing to the byte span of the text that
//! stands for it: a Skill Invocation, written as `$skill-name`, or an
//! Attachment, written as its label such as `[Image 1]`. The span bookkeeping
//! every kind shares lives in [`TextBindings`]: bindings stay in text order, a
//! span moves with an edit made wholly before it, an edit reaching into a span
//! drops its binding, and a Prompt's bindings are raised from, and lowered
//! back into, the lists the protocol carries beside its text. A kind supplies
//! only what is its own: how the text at its span is recognized, how that span
//! is drawn, and whether the span is one unit the cursor steps over and
//! deletion takes whole.

mod attachment;
mod skill;

use std::ops::Range;

use ratatui::style::Style;

use crate::{
    protocol::{AttachmentBinding, InitialPrompt, Message, PromptId, SkillInvocation},
    theme::Theme,
};

pub(super) use attachment::{BoundAttachment, attachment_name, image_label};
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
    /// An uploaded Attachment, standing in the text as its label.
    Attachment(attachment::BoundAttachment),
}

impl BindingKind {
    /// Whether `text`, found at a binding's span, still stands for what the
    /// span is bound to.
    fn is_written_as(&self, text: &str) -> bool {
        match self {
            Self::Skill(skill) => skill.is_written_as(text),
            Self::Attachment(attachment) => attachment.is_written_as(text),
        }
    }

    /// The style bound text is drawn in over the plain text around it.
    pub(super) fn style(&self, theme: &Theme) -> Style {
        match self {
            Self::Skill(_) | Self::Attachment(_) => theme.accent.primary,
        }
    }

    /// Whether the span is one unit: the cursor steps over it whole, a
    /// selection covers all of it or none, and deleting into it deletes it,
    /// binding and all. A Skill Invocation is edited as the text it is.
    fn is_unit(&self) -> bool {
        match self {
            Self::Skill(_) => false,
            Self::Attachment(_) => true,
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
            BindingKind::Attachment(_) => None,
        }
    }

    fn skill_mut(&mut self) -> Option<(&Range<usize>, &mut skill::BoundSkill)> {
        match &mut self.kind {
            BindingKind::Skill(skill) => Some((&self.span, skill)),
            BindingKind::Attachment(_) => None,
        }
    }

    fn attachment(&self) -> Option<(&Range<usize>, &attachment::BoundAttachment)> {
        match &self.kind {
            BindingKind::Attachment(attachment) => Some((&self.span, attachment)),
            BindingKind::Skill(_) => None,
        }
    }
}

/// The bindings beside one text, in text order.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct TextBindings(Vec<TextBinding>);

impl TextBindings {
    /// The bindings a Prompt carries beside its text.
    pub(super) fn from_prompt(prompt: &InitialPrompt) -> Self {
        Self::from_lists(&prompt.skill_invocations, &prompt.attachments)
    }

    /// The bindings a Message carries beside its content.
    pub(super) fn from_message(message: &Message) -> Self {
        Self::from_lists(&message.skill_invocations, &message.attachments)
    }

    fn from_lists(
        skill_invocations: &[SkillInvocation],
        attachments: &[AttachmentBinding],
    ) -> Self {
        let mut bindings = skill_invocations
            .iter()
            .map(skill::BoundSkill::binding)
            .chain(attachments.iter().map(attachment::BoundAttachment::binding))
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
            attachments: self.attachment_bindings(),
        }
    }

    /// Whether these are exactly the bindings `prompt` carries beside its text.
    pub(super) fn are_carried_by(&self, prompt: &InitialPrompt) -> bool {
        self.skill_invocations() == prompt.skill_invocations
            && self.attachment_bindings() == prompt.attachments
    }

    fn skill_invocations(&self) -> Vec<SkillInvocation> {
        self.skills()
            .map(|(span, skill)| skill.invocation(span))
            .collect()
    }

    fn attachment_bindings(&self) -> Vec<AttachmentBinding> {
        self.attachments()
            .map(|(span, attachment)| attachment.attachment_binding(span))
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

    /// The unit a cursor at `offset` steps back over, or a backward deletion
    /// there takes: one ending at `offset` or standing around it.
    pub(super) fn unit_before(&self, offset: usize) -> Option<Range<usize>> {
        self.units()
            .find(|span| span.start < offset && offset <= span.end)
    }

    /// The unit a cursor at `offset` steps forward over, or a forward deletion
    /// there takes: one starting at `offset` or standing around it.
    pub(super) fn unit_after(&self, offset: usize) -> Option<Range<usize>> {
        self.units()
            .find(|span| span.start <= offset && offset < span.end)
    }

    /// Where an offset lands once it is kept out of the middle of any unit:
    /// unchanged between units, otherwise moved to the unit's edge in the
    /// direction given, or to the nearer edge.
    pub(super) fn unit_boundary(&self, offset: usize, toward: UnitEdge) -> usize {
        let Some(unit) = self
            .units()
            .find(|span| span.start < offset && offset < span.end)
        else {
            return offset;
        };
        match toward {
            UnitEdge::Start => unit.start,
            UnitEdge::End => unit.end,
            UnitEdge::Nearer if offset - unit.start < unit.end - offset => unit.start,
            UnitEdge::Nearer => unit.end,
        }
    }

    /// `range` widened to take whole every unit it reaches into.
    pub(super) fn covering_units(&self, range: Range<usize>) -> Range<usize> {
        let reached = range.clone();
        self.units()
            .filter(|span| span.start < reached.end && reached.start < span.end)
            .fold(range, |range, span| {
                range.start.min(span.start)..range.end.max(span.end)
            })
    }

    fn units(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        self.0
            .iter()
            .filter(|binding| binding.kind.is_unit())
            .map(|binding| binding.span.clone())
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

/// Which edge of a unit an offset standing inside it moves to.
#[derive(Clone, Copy, Debug)]
pub(super) enum UnitEdge {
    Start,
    End,
    Nearer,
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

    fn attached(label: &str, span: Range<usize>) -> AttachmentBinding {
        AttachmentBinding {
            attachment_id: crate::protocol::AttachmentId::new(format!("{label}-id")),
            label: label.to_owned(),
            span: TextSpan::from(span),
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

    #[test]
    fn attachments_raise_and_lower_beside_skills_in_text_order() {
        let mut original = prompt(
            "[Image 1] and $review then [Image 2]",
            vec![invocation("review", 14..21)],
        );
        original.attachments = vec![attached("[Image 2]", 27..36), attached("[Image 1]", 0..9)];
        let bindings = TextBindings::from_prompt(&original);
        assert_eq!(spans(&bindings), vec![0..9, 14..21, 27..36]);
        assert_eq!(bindings.highest_image_number(), 2);

        let lowered = bindings.into_prompt(original.id, original.text.clone());
        assert_eq!(
            lowered.attachments,
            vec![attached("[Image 1]", 0..9), attached("[Image 2]", 27..36)]
        );
        assert_eq!(lowered.skill_invocations, original.skill_invocations);
        assert!(TextBindings::from_prompt(&lowered).are_carried_by(&lowered));

        let mut without_one = lowered.clone();
        without_one.attachments.pop();
        assert!(!TextBindings::from_prompt(&lowered).are_carried_by(&without_one));
    }

    #[test]
    fn dropping_an_attachment_leaves_its_label_unbound_and_every_other_binding() {
        let text = "[Image 1] $review [Image 2]";
        let mut carried = prompt(text, vec![invocation("review", 10..17)]);
        carried.attachments = vec![attached("[Image 1]", 0..9), attached("[Image 2]", 18..27)];
        let mut bindings = TextBindings::from_prompt(&carried);

        let dropped = bindings.drop_attachments(|attachment| attachment.label() == "[Image 1]");
        assert_eq!(
            dropped
                .iter()
                .map(BoundAttachment::label)
                .collect::<Vec<_>>(),
            vec!["[Image 1]"]
        );
        assert_eq!(spans(&bindings), vec![10..17, 18..27]);
        assert_eq!(bindings.highest_image_number(), 2);
        assert_eq!(bindings.unit_after(0), None, "the label is plain text now");
        assert!(bindings.drop_attachments(|_| false).is_empty());
    }

    #[test]
    fn an_attachment_label_is_recognized_only_as_written() {
        let mut carried = prompt("[Image 1]", Vec::new());
        carried.attachments = vec![attached("[Image 1]", 0..9)];
        let bindings = TextBindings::from_prompt(&carried);
        assert_eq!(bindings.recognized_in("[Image 1]").count(), 1);
        assert_eq!(bindings.recognized_in("[Image 7]").count(), 0);
        assert_eq!(bindings.recognized_in("[image 1]").count(), 0);
    }

    #[test]
    fn only_an_attachment_label_is_a_unit_of_its_text() {
        // "$review [Image 1] x"
        let mut carried = prompt("$review [Image 1] x", vec![invocation("review", 0..7)]);
        carried.attachments = vec![attached("[Image 1]", 8..17)];
        let bindings = TextBindings::from_prompt(&carried);

        assert_eq!(bindings.unit_before(7), None);
        assert_eq!(bindings.unit_after(0), None);
        assert_eq!(bindings.unit_before(8), None);
        assert_eq!(bindings.unit_after(8), Some(8..17));
        assert_eq!(bindings.unit_after(12), Some(8..17));
        assert_eq!(bindings.unit_after(17), None);
        assert_eq!(bindings.unit_before(17), Some(8..17));
        assert_eq!(bindings.unit_before(12), Some(8..17));

        assert_eq!(bindings.unit_boundary(3, UnitEdge::Nearer), 3);
        assert_eq!(bindings.unit_boundary(8, UnitEdge::End), 8);
        assert_eq!(bindings.unit_boundary(10, UnitEdge::Nearer), 8);
        assert_eq!(bindings.unit_boundary(14, UnitEdge::Nearer), 17);
        assert_eq!(bindings.unit_boundary(10, UnitEdge::End), 17);
        assert_eq!(bindings.unit_boundary(14, UnitEdge::Start), 8);

        assert_eq!(bindings.covering_units(2..5), 2..5);
        assert_eq!(bindings.covering_units(5..10), 5..17);
        assert_eq!(bindings.covering_units(12..19), 8..19);
        assert_eq!(bindings.covering_units(17..19), 17..19);
    }
}
