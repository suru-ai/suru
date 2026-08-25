use std::sync::Arc;

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::protocol::SessionId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Registration is crate-private until the deferred plugin loader exists.
pub(super) enum Placement {
    Prepend,
    Replace,
    Append,
}

#[derive(Clone, Debug)]
pub(super) struct SlotText {
    pub(super) text: String,
    pub(super) style: Style,
}

impl SlotText {
    pub(super) fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct LandingNoticeSlotContext {
    pub(super) width: u16,
    /// The Notice the Landing has to carry, or `None` when startup found
    /// nothing to report and the built-in content is no row at all.
    pub(super) notice: Option<SlotText>,
}

#[derive(Clone, Debug)]
pub(super) struct LandingFooterSlotContext {
    pub(super) width: u16,
    pub(super) context: SlotText,
    pub(super) connection: SlotText,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SessionComposerTopSlotContext {
    pub(super) session_id: SessionId,
    pub(super) width: u16,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PromptFooterSlotContext {
    pub(super) session_id: SessionId,
    pub(super) width: u16,
}

#[derive(Clone, Debug)]
pub(super) struct PromptStatusSlotContext {
    pub(super) session_id: SessionId,
    pub(super) status: SlotText,
}

#[derive(Clone, Debug)]
pub(super) struct PromptContextSlotContext {
    pub(super) session_id: SessionId,
    pub(super) agent: SlotText,
    pub(super) connection: SlotText,
}

#[derive(Clone, Debug)]
pub(super) struct SlotFailure {
    pub(super) slot: &'static str,
    pub(super) message: String,
}

#[derive(Clone, Debug)]
pub(super) struct RenderedSlot<Content> {
    pub(super) content: Vec<Content>,
    pub(super) failures: Vec<SlotFailure>,
}

impl<Content> RenderedSlot<Content> {
    pub(super) fn height(&self) -> u16 {
        self.content
            .len()
            .saturating_add(self.failures.len())
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

type Renderer<Context, Content> =
    Arc<dyn Fn(&Context) -> Result<Vec<Content>, String> + Send + Sync>;

struct Contribution<Context, Content> {
    placement: Placement,
    render: Renderer<Context, Content>,
}

struct Slot<Context, Content> {
    name: &'static str,
    contributions: Vec<Contribution<Context, Content>>,
}

impl<Context, Content> Slot<Context, Content> {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            contributions: Vec::new(),
        }
    }

    #[allow(dead_code)] // Production currently registers built-in content only.
    fn contribute(
        &mut self,
        placement: Placement,
        render: impl Fn(&Context) -> Result<Vec<Content>, String> + Send + Sync + 'static,
    ) {
        self.contributions.push(Contribution {
            placement,
            render: Arc::new(render),
        });
    }

    fn compose(&self, context: &Context, default: Vec<Content>) -> RenderedSlot<Content> {
        let mut prepended = Vec::new();
        let mut replacement = None;
        let mut appended = Vec::new();
        let mut failures = Vec::new();

        for contribution in &self.contributions {
            match (contribution.render)(context) {
                Ok(content) => match contribution.placement {
                    Placement::Prepend => prepended.extend(content),
                    Placement::Replace => replacement = Some(content),
                    Placement::Append => appended.extend(content),
                },
                Err(message) => failures.push(SlotFailure {
                    slot: self.name,
                    message,
                }),
            }
        }

        prepended.extend(replacement.unwrap_or(default));
        prepended.extend(appended);
        RenderedSlot {
            content: prepended,
            failures,
        }
    }
}

#[derive(Clone, Copy)]
enum FooterSide {
    Left,
    Right,
}

#[derive(Clone)]
struct FooterItem {
    side: FooterSide,
    content: SlotText,
}

pub(super) struct RenderSlots {
    landing_notice: Slot<LandingNoticeSlotContext, Line<'static>>,
    landing_footer: Slot<LandingFooterSlotContext, Line<'static>>,
    session_composer_top: Slot<SessionComposerTopSlotContext, Line<'static>>,
    prompt_footer: Slot<PromptFooterSlotContext, Line<'static>>,
    prompt_footer_status: Slot<PromptStatusSlotContext, FooterItem>,
    prompt_footer_context: Slot<PromptContextSlotContext, FooterItem>,
}

impl Default for RenderSlots {
    fn default() -> Self {
        Self {
            landing_notice: Slot::new("landing.notice"),
            landing_footer: Slot::new("landing.footer"),
            session_composer_top: Slot::new("session.composer.top"),
            prompt_footer: Slot::new("prompt.footer"),
            prompt_footer_status: Slot::new("prompt.footer.status"),
            prompt_footer_context: Slot::new("prompt.footer.context"),
        }
    }
}

impl RenderSlots {
    pub(super) fn builtins() -> Self {
        Self::default()
    }

    pub(super) fn landing_notice(
        &self,
        context: &LandingNoticeSlotContext,
    ) -> RenderedSlot<Line<'static>> {
        let default = context
            .notice
            .iter()
            .map(|notice| one_line(context.width, notice))
            .collect();
        self.landing_notice.compose(context, default)
    }

    pub(super) fn landing_footer(
        &self,
        context: &LandingFooterSlotContext,
    ) -> RenderedSlot<Line<'static>> {
        self.landing_footer.compose(
            context,
            vec![spread_line(
                context.width,
                &context.context,
                &context.connection,
            )],
        )
    }

    pub(super) fn session_composer_top(
        &self,
        context: &SessionComposerTopSlotContext,
    ) -> RenderedSlot<Line<'static>> {
        let _ = (context.session_id, context.width);
        self.session_composer_top.compose(context, Vec::new())
    }

    pub(super) fn prompt_footer(
        &self,
        footer: &PromptFooterSlotContext,
        status: &PromptStatusSlotContext,
        context: &PromptContextSlotContext,
    ) -> RenderedSlot<Line<'static>> {
        debug_assert_eq!(footer.session_id, status.session_id);
        debug_assert_eq!(footer.session_id, context.session_id);
        let status = self.prompt_footer_status.compose(
            status,
            vec![FooterItem {
                side: FooterSide::Left,
                content: status.status.clone(),
            }],
        );
        let context = self.prompt_footer_context.compose(
            context,
            vec![
                FooterItem {
                    side: FooterSide::Left,
                    content: context.agent.clone(),
                },
                FooterItem {
                    side: FooterSide::Right,
                    content: context.connection.clone(),
                },
            ],
        );
        let mut failures = status.failures;
        failures.extend(context.failures);
        let default = vec![spread_footer_items(
            footer.width,
            status.content.into_iter().chain(context.content),
        )];
        let mut rendered = self.prompt_footer.compose(footer, default);
        failures.append(&mut rendered.failures);
        rendered.failures = failures;
        rendered
    }
}

/// One row of slot text, cut to the width it has rather than wrapping: a
/// Notice that grew a second row would push the Landing around.
fn one_line(width: u16, text: &SlotText) -> Line<'static> {
    Line::from(
        truncate_slot_text(vec![text.clone()], usize::from(width))
            .into_iter()
            .map(|item| Span::styled(item.text, item.style))
            .collect::<Vec<_>>(),
    )
}

fn spread_footer_items(width: u16, items: impl IntoIterator<Item = FooterItem>) -> Line<'static> {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for item in items {
        match item.side {
            FooterSide::Left => left.push(item.content),
            FooterSide::Right => right.push(item.content),
        }
    }
    spread_slot_text(width, join_slot_text(left), join_slot_text(right))
}

fn join_slot_text(items: Vec<SlotText>) -> Vec<SlotText> {
    let mut joined = Vec::new();
    for item in items.into_iter().filter(|item| !item.text.is_empty()) {
        if !joined.is_empty() {
            joined.push(SlotText::new(" · ", Style::default()));
        }
        joined.push(item);
    }
    joined
}

fn spread_line(width: u16, left: &SlotText, right: &SlotText) -> Line<'static> {
    spread_slot_text(width, vec![left.clone()], vec![right.clone()])
}

fn spread_slot_text(width: u16, left: Vec<SlotText>, right: Vec<SlotText>) -> Line<'static> {
    let width = usize::from(width);
    let left = left
        .into_iter()
        .filter(|item| !item.text.is_empty())
        .collect::<Vec<_>>();
    let right = right
        .into_iter()
        .filter(|item| !item.text.is_empty())
        .collect::<Vec<_>>();
    let has_left = !left.is_empty();
    let has_right = !right.is_empty();
    let right = truncate_slot_text(right, width);
    let right_width = slot_text_width(&right);
    let gap = usize::from(has_left && has_right) * 2;
    let left_width = width.saturating_sub(right_width.saturating_add(gap));
    let left = truncate_slot_text(left, left_width);
    let spacing = " ".repeat(width.saturating_sub(slot_text_width(&left) + right_width));
    let spans = left
        .into_iter()
        .map(|item| Span::styled(item.text, item.style))
        .chain(std::iter::once(Span::raw(spacing)))
        .chain(
            right
                .into_iter()
                .map(|item| Span::styled(item.text, item.style)),
        )
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn slot_text_width(items: &[SlotText]) -> usize {
    items.iter().map(|item| item.text.width()).sum()
}

fn truncate_slot_text(items: Vec<SlotText>, width: usize) -> Vec<SlotText> {
    if slot_text_width(&items) <= width {
        return items;
    }
    if width == 0 {
        return Vec::new();
    }

    let suffix = if width > 1 { "…" } else { "" };
    let content_width = width.saturating_sub(suffix.width());
    let mut result = Vec::new();
    let mut used = 0;
    let mut suffix_style = Style::default();

    for item in items {
        suffix_style = item.style;
        if used >= content_width {
            break;
        }
        let mut text = String::new();
        let mut reached_limit = false;
        for character in item.text.chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > content_width {
                reached_limit = true;
                break;
            }
            text.push(character);
            used += character_width;
        }
        if !text.is_empty() {
            result.push(SlotText::new(text, item.style));
        }
        if reached_limit {
            break;
        }
    }
    if !suffix.is_empty() {
        result.push(SlotText::new(suffix, suffix_style));
    }
    result
}

pub(super) fn truncate_to_width(value: &str, width: usize) -> String {
    truncate_slot_text(vec![SlotText::new(value, Style::default())], width)
        .into_iter()
        .map(|item| item.text)
        .collect()
}

#[cfg(test)]
pub(super) struct TestContribution {
    slot: TestSlot,
    placement: Placement,
    result: Result<&'static str, &'static str>,
    style: Style,
}

#[cfg(test)]
enum TestSlot {
    LandingNotice,
    LandingFooter,
    SessionComposerTop,
    PromptFooter,
    PromptFooterStatus,
    PromptFooterContext,
}

#[cfg(test)]
impl TestContribution {
    pub(super) fn landing_notice(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::LandingNotice,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn landing_footer(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::LandingFooter,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn session_composer_top(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::SessionComposerTop,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn prompt_footer(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::PromptFooter,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn prompt_footer_status(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::PromptFooterStatus,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn prompt_footer_context(
        placement: Placement,
        result: Result<&'static str, &'static str>,
    ) -> Self {
        Self {
            slot: TestSlot::PromptFooterContext,
            placement,
            result,
            style: Style::default(),
        }
    }

    pub(super) fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

#[cfg(test)]
impl RenderSlots {
    pub(super) fn testing<const N: usize>(contributions: [TestContribution; N]) -> Self {
        let mut slots = Self::builtins();
        for contribution in contributions {
            let result = contribution.result;
            let style = contribution.style;
            match contribution.slot {
                TestSlot::LandingNotice => {
                    slots
                        .landing_notice
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![Line::styled(text, style)]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
                TestSlot::LandingFooter => {
                    slots
                        .landing_footer
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![Line::styled(text, style)]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
                TestSlot::SessionComposerTop => {
                    slots
                        .session_composer_top
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![Line::styled(text, style)]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
                TestSlot::PromptFooter => {
                    slots
                        .prompt_footer
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![Line::styled(text, style)]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
                TestSlot::PromptFooterStatus => {
                    slots
                        .prompt_footer_status
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![FooterItem {
                                side: FooterSide::Left,
                                content: SlotText::new(text, style),
                            }]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
                TestSlot::PromptFooterContext => {
                    slots
                        .prompt_footer_context
                        .contribute(contribution.placement, move |_| match result {
                            Ok(text) => Ok(vec![FooterItem {
                                side: FooterSide::Left,
                                content: SlotText::new(text, style),
                            }]),
                            Err(message) => Err(message.to_owned()),
                        })
                }
            }
        }
        slots
    }

    pub(super) fn testing_session_column_widths() -> Self {
        let mut slots = Self::builtins();
        slots
            .session_composer_top
            .contribute(Placement::Append, |context| {
                Ok(vec![Line::raw(format!(
                    "composer extension width {}",
                    context.width
                ))])
            });
        slots
            .prompt_footer
            .contribute(Placement::Append, |context| {
                Ok(vec![Line::raw(format!(
                    "footer extension width {}",
                    context.width
                ))])
            });
        slots
    }
}
