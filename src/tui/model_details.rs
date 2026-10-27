//! What the Model picker tells about the Model focus stands on, beyond the row
//! that lists it: the Model Advisories its Provider asks be shown, its token
//! prices per context tier, its limits, and its Model Options. A wide picker
//! draws all of it in a pane beside the list; a narrow one a two-line strip
//! beneath it.

use ratatui::{style::Modifier, text::Line};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{
        ModelAdvisory, ModelAdvisorySeverity, ModelDescriptor, ModelOptionKind, ModelPrice,
        ModelPriceCategory, ModelPriceTier,
    },
    theme::Theme,
};

use super::{
    selection::WrappedLine, slots::truncate_to_width, usage::compact_count,
    wrapped_text::WrappedText,
};

/// How far an advisory's text and a price table stand in beneath their
/// headings, and an untitled warning's text beside its marker.
const INDENT: &str = "  ";

/// The glyph a list row compares a Model's relative price with.
pub(super) fn price_glyph(category: ModelPriceCategory) -> &'static str {
    price_category(category).0
}

/// A relative price as a glyph for comparing rows and as words for reading.
fn price_category(category: ModelPriceCategory) -> (&'static str, &'static str) {
    match category {
        ModelPriceCategory::Low => ("$", "Low"),
        ModelPriceCategory::Medium => ("$$", "Medium"),
        ModelPriceCategory::High => ("$$$", "High"),
        ModelPriceCategory::VeryHigh => ("$$$$", "Very high"),
    }
}

/// The pane a wide picker details its focused Model in. It holds each
/// advisory's text as the pane writes it, so a selection across a wrapped
/// advisory reads it back as the one line it was written as (ADR-0020).
pub(super) struct DetailsPane<'a> {
    model: &'a ModelDescriptor,
    identity: String,
    advisories: Vec<(&'a ModelAdvisory, String)>,
}

/// The rows a pane drew, and each line it wrapped over rows of its own.
pub(super) struct DrawnPane<'a> {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) wrapped: Vec<WrappedLine<'a>>,
}

impl<'a> DetailsPane<'a> {
    pub(super) fn new(model: &'a ModelDescriptor) -> Self {
        Self {
            model,
            identity: identity(model),
            advisories: model
                .details
                .advisories
                .iter()
                .map(|advisory| (advisory, plain_links(&advisory.text)))
                .collect(),
        }
    }

    /// The pane in `height` rows `width` columns wide. A pane with more to
    /// say than it has rows for ends on an ellipsis rather than silently.
    pub(super) fn draw(&self, width: u16, height: usize, theme: &Theme) -> DrawnPane<'_> {
        let mut drawn = DrawnPane {
            lines: Vec::new(),
            wrapped: Vec::new(),
        };
        let line =
            |text: &str, style| Line::styled(truncate_to_width(text, usize::from(width)), style);
        drawn.lines.push(line(
            &self.model.display_name,
            theme.text.primary.add_modifier(Modifier::BOLD),
        ));
        drawn.wrap(&self.identity, "", width, theme.text.subdued);
        for (advisory, text) in &self.advisories {
            drawn.lines.push(Line::default());
            let (marker, style) = match advisory.severity {
                ModelAdvisorySeverity::Warning => ("! ", theme.feedback.warning),
                ModelAdvisorySeverity::Info => ("", theme.text.subdued),
            };
            match &advisory.title {
                Some(title) => {
                    drawn.lines.push(line(
                        &format!("{marker}{title}"),
                        style.add_modifier(Modifier::BOLD),
                    ));
                    drawn.wrap(text, INDENT, width, theme.text.primary);
                }
                // An untitled advisory is its text alone, a warning's marker
                // standing where its indent would.
                None if marker.is_empty() => drawn.wrap(text, "", width, style),
                None => {
                    let first = drawn.lines.len();
                    drawn.wrap(text, INDENT, width, style);
                    if let Some(row) = drawn.lines.get_mut(first) {
                        let text = row.to_string();
                        *row = Line::styled(format!("{marker}{}", &text[INDENT.len()..]), style);
                    }
                }
            }
        }
        if let Some(prices) = &self.model.details.prices {
            drawn.lines.push(Line::default());
            drawn.lines.push(line(
                &format!(
                    "{} per {} tokens",
                    prices.unit,
                    compact_count(prices.per_tokens)
                ),
                theme.text.subdued,
            ));
            for row in price_table(&prices.tiers) {
                drawn.lines.push(line(&row, theme.text.primary));
            }
        }
        let specifics = limits_and_options(self.model);
        if !specifics.is_empty() {
            drawn.lines.push(Line::default());
            let label_width = specifics
                .iter()
                .map(|(label, _)| label.width())
                .max()
                .unwrap_or(0);
            for (label, value) in specifics {
                drawn.lines.push(line(
                    &format!("{label:<label_width$}  {value}"),
                    theme.text.primary,
                ));
            }
        }
        drawn.fit(height, theme);
        drawn
    }
}

impl<'a> DrawnPane<'a> {
    /// `text` broken over rows of its own `width` columns wide.
    fn wrap(
        &mut self,
        text: &'a str,
        indent: &'static str,
        width: u16,
        style: ratatui::style::Style,
    ) {
        if text.is_empty() {
            return;
        }
        let wrapped = WrappedText::new(text, indent, width);
        self.wrapped.push(wrapped.wrapped(self.lines.len()));
        self.lines.extend(wrapped.lines(style));
    }

    /// The pane cut to `height` rows, its last row an ellipsis where it had
    /// more, and each wrapped line to the rows still drawn.
    fn fit(&mut self, height: usize, theme: &Theme) {
        if self.lines.len() <= height {
            return;
        }
        let kept = height.saturating_sub(1);
        self.lines.truncate(kept);
        if height > 0 {
            self.lines.push(Line::styled("…", theme.text.subdued));
        }
        self.wrapped.retain_mut(|line| {
            let rows = kept.saturating_sub(line.row);
            line.parts.truncate(rows);
            !line.parts.is_empty()
        });
    }
}

/// The two lines a narrow picker spares beneath its list for the Model focus
/// stands on: what it is, then its most pressing advisory or, failing one, its
/// prices. Focus on no Model leaves them blank, so the list keeps its height
/// as focus moves.
pub(super) fn strip_lines(
    model: Option<&ModelDescriptor>,
    width: u16,
    theme: &Theme,
) -> [Line<'static>; 2] {
    let Some(model) = model else {
        return [Line::default(), Line::default()];
    };
    let width = usize::from(width);
    let details = &model.details;
    let second = if let Some(advisory) = details
        .advisories
        .iter()
        .find(|advisory| advisory.severity == ModelAdvisorySeverity::Warning)
    {
        let text = advisory.title.as_deref().unwrap_or(&advisory.text);
        Line::styled(
            truncate_to_width(&format!("! {}", plain_links(text)), width),
            theme.feedback.warning,
        )
    } else if let Some(prices) = &details.prices {
        Line::styled(
            truncate_to_width(&price_summary(&prices.tiers), width),
            theme.text.primary,
        )
    } else {
        Line::default()
    };
    [
        Line::styled(
            truncate_to_width(&identity(model), width),
            theme.text.subdued,
        ),
        second,
    ]
}

/// The Model's identifier, its Provider's account of it, and its relative
/// price, on one line.
fn identity(model: &ModelDescriptor) -> String {
    let mut parts = vec![model.id.to_string()];
    if !model.description.is_empty() {
        parts.push(model.description.clone());
    }
    if let Some(category) = model.details.price_category {
        parts.push(format!("{} price", price_category(category).1));
    }
    parts.join(" · ")
}

/// `markdown` with each `[text](address)` link written `text (address)`: the
/// picker cannot be clicked through, so an address is only any use spelled
/// out.
fn plain_links(markdown: &str) -> String {
    let mut plain = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(open) = rest.find('[') {
        let link = rest[open + 1..].split_once("](").and_then(|(text, after)| {
            let (address, after) = after.split_once(')')?;
            (!text.contains('[')).then_some((text, address, after))
        });
        let Some((text, address, after)) = link else {
            plain.push_str(&rest[..=open]);
            rest = &rest[open + 1..];
            continue;
        };
        plain.push_str(&rest[..open]);
        plain.push_str(&format!("{text} ({address})"));
        rest = after;
    }
    plain.push_str(rest);
    plain
}

/// The price table: a column per tier, headed by the tier's name where there
/// is more than one, and a row per price some tier publishes.
fn price_table(tiers: &[ModelPriceTier]) -> Vec<String> {
    type Cell = fn(&ModelPriceTier) -> Option<String>;
    fn price(price: Option<ModelPrice>) -> Option<String> {
        price.map(format_price)
    }
    let rows: [(&str, Cell); 6] = [
        ("Prompt limit", |tier| {
            tier.max_prompt_tokens.map(compact_count)
        }),
        ("Input", |tier| price(tier.input)),
        ("Output", |tier| price(tier.output)),
        ("Cache read", |tier| price(tier.cache_read)),
        ("Cache write", |tier| price(tier.cache_write)),
        ("Cache write 1h", |tier| price(tier.cache_write_1h)),
    ];
    let rows = rows
        .into_iter()
        .map(|(label, cell)| (label, tiers.iter().map(cell).collect::<Vec<_>>()))
        .filter(|(_, cells)| cells.iter().any(Option::is_some))
        .collect::<Vec<_>>();
    let label_width = rows
        .iter()
        .map(|(label, _)| label.width())
        .max()
        .unwrap_or(0);
    let headed = tiers.len() > 1;
    let column_widths = tiers
        .iter()
        .enumerate()
        .map(|(column, tier)| {
            rows.iter()
                .filter_map(|(_, cells)| cells[column].as_deref().map(UnicodeWidthStr::width))
                .chain(headed.then(|| tier.label.width()))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let row = |label: &str, cells: Vec<&str>| {
        let mut row = format!("{INDENT}{label:<label_width$}");
        for (cell, width) in cells.into_iter().zip(&column_widths) {
            row.push_str(&format!("   {cell:>width$}"));
        }
        row.trim_end().to_owned()
    };
    let mut table = Vec::new();
    if headed {
        table.push(row(
            "",
            tiers.iter().map(|tier| tier.label.as_str()).collect(),
        ));
    }
    for (label, cells) in &rows {
        table.push(row(
            label,
            cells
                .iter()
                .map(|cell| cell.as_deref().unwrap_or("–"))
                .collect(),
        ));
    }
    table
}

/// Every tier's prices on one line: the first as the Model's own, each other
/// after its name.
fn price_summary(tiers: &[ModelPriceTier]) -> String {
    let summary = |tier: &ModelPriceTier| {
        let mut parts = Vec::new();
        if let Some(input) = tier.input {
            parts.push(format!("In {}", format_price(input)));
        }
        if let Some(output) = tier.output {
            parts.push(format!("Out {}", format_price(output)));
        }
        let cache = [tier.cache_read, tier.cache_write]
            .into_iter()
            .flatten()
            .map(format_price)
            .collect::<Vec<_>>();
        if !cache.is_empty() {
            parts.push(format!("Cache {}", cache.join("/")));
        }
        parts.join(" · ")
    };
    let mut tiers = tiers.iter();
    let Some(first) = tiers.next() else {
        return String::new();
    };
    let mut line = summary(first);
    for tier in tiers {
        line.push_str(&format!("  ({}: {})", tier.label, summary(tier)));
    }
    line
}

/// A price as its Provider would write it: no more decimals than it has.
fn format_price(price: ModelPrice) -> String {
    let formatted = format!("{:.6}", price.as_units());
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// The Model's limits and what each of its Model Options offers, as labelled
/// values.
fn limits_and_options(model: &ModelDescriptor) -> Vec<(String, String)> {
    let details = &model.details;
    let mut specifics = Vec::new();
    if let Some(tokens) = details.context_window_tokens {
        specifics.push(("Context window".to_owned(), compact_count(tokens)));
    }
    if let Some(tokens) = details.output_limit_tokens {
        specifics.push(("Output limit".to_owned(), compact_count(tokens)));
    }
    if let Some(accepts) = details.accepts_images {
        let value = if accepts { "Accepted" } else { "Not accepted" };
        specifics.push(("Images".to_owned(), value.to_owned()));
    }
    for option in &model.options {
        let offered = match &option.kind {
            ModelOptionKind::Select { choices, .. } => match choices.as_slice() {
                [] => continue,
                [first, .., last] if choices.len() > 3 => {
                    format!("{} … {}", first.label, last.label)
                }
                choices => choices
                    .iter()
                    .map(|choice| choice.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            ModelOptionKind::Toggle { .. } => "On or off".to_owned(),
        };
        specifics.push((option.label.clone(), offered));
    }
    specifics
}
