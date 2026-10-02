//! View state for the Context Breakdown a reader asked the open Session's Provider for.

use std::cell::Cell;

use uuid::Uuid;

use crate::protocol::{ContextBreakdown, ContextFill, ContextItem, ContextSource};

use super::usage::compact_count;

/// How many of a source's items are named before the rest are summed into one row.
const NAMED_ITEMS: usize = 5;

/// Why a Session's Provider gave no Context Breakdown, as the overlay explains it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextBreakdownRefusal {
    /// The Provider attributes none of its context.
    Unsupported,
    /// The request failed, for the reason given.
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ContextOverlay {
    state: OverlayState,
    /// The Session's own Context Fill when the reader asked: what an
    /// unsupported Provider still reports, and the window a breakdown that
    /// names none is measured against.
    fill: Option<ContextFill>,
    /// The display name of the Provider asked, where the client knows it.
    provider: Option<String>,
    scroll: usize,
    /// How many rows the last draw showed, which a page scrolls by and the
    /// scroll is held within.
    viewport: Cell<usize>,
}

#[derive(Clone, Debug, Default)]
enum OverlayState {
    #[default]
    Closed,
    Reading {
        request_id: Uuid,
    },
    Read(ContextBreakdown),
    Refused(ContextBreakdownRefusal),
}

/// What the overlay shows.
pub(super) enum ContextOverlayView<'a> {
    Reading,
    Rows(Vec<ContextRow>),
    Unsupported {
        provider: Option<&'a str>,
        fill: Option<String>,
    },
    Failed(&'a str),
}

/// One row of a breakdown, already reduced to the text it shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContextRow {
    /// The occupancy the breakdown divides, against its window where known.
    Fill(String),
    Blank,
    /// A source's share, with its percentage of the window where known.
    Part {
        label: String,
        tokens: String,
        share: Option<String>,
    },
    /// An item of the source above.
    Item {
        label: String,
        tokens: String,
    },
    /// The window held back, or left free, rather than occupied.
    Capacity {
        label: &'static str,
        tokens: String,
        share: String,
    },
}

impl ContextOverlay {
    pub(super) fn open(
        &mut self,
        request_id: Uuid,
        fill: Option<ContextFill>,
        provider: Option<String>,
    ) {
        self.state = OverlayState::Reading { request_id };
        self.fill = fill;
        self.provider = provider;
        self.scroll = 0;
    }

    pub(super) fn close(&mut self) {
        self.state = OverlayState::Closed;
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self.state, OverlayState::Closed)
    }

    /// Takes the Provider's answer, unless the reader has since closed the
    /// overlay or asked again.
    pub(super) fn receive(
        &mut self,
        request_id: Uuid,
        result: Result<ContextBreakdown, ContextBreakdownRefusal>,
    ) {
        if !matches!(self.state, OverlayState::Reading { request_id: asked } if asked == request_id)
        {
            return;
        }
        self.state = match result {
            Ok(breakdown) => OverlayState::Read(breakdown),
            Err(refusal) => OverlayState::Refused(refusal),
        };
    }

    pub(super) fn scroll_by(&mut self, rows: isize) {
        self.scroll = self.scroll.saturating_add_signed(rows);
    }

    pub(super) fn page_by(&mut self, pages: isize) {
        let page = isize::try_from(self.viewport.get().max(1)).unwrap_or(isize::MAX);
        self.scroll_by(pages.saturating_mul(page));
    }

    /// The first row a draw `viewport` rows tall shows of `rows`, keeping the
    /// last page full however far the reader scrolled.
    pub(super) fn first_row(&self, rows: usize, viewport: usize) -> usize {
        self.viewport.set(viewport);
        self.scroll.min(rows.saturating_sub(viewport))
    }

    pub(super) fn view(&self) -> ContextOverlayView<'_> {
        match &self.state {
            OverlayState::Closed | OverlayState::Reading { .. } => ContextOverlayView::Reading,
            OverlayState::Read(breakdown) => ContextOverlayView::Rows(rows(breakdown, self.fill)),
            OverlayState::Refused(ContextBreakdownRefusal::Unsupported) => {
                ContextOverlayView::Unsupported {
                    provider: self.provider.as_deref(),
                    fill: self.fill.map(|fill| fill_text(fill, fill.capacity_tokens)),
                }
            }
            OverlayState::Refused(ContextBreakdownRefusal::Failed(message)) => {
                ContextOverlayView::Failed(message)
            }
        }
    }
}

fn rows(breakdown: &ContextBreakdown, session_fill: Option<ContextFill>) -> Vec<ContextRow> {
    let capacity = window(breakdown.fill.capacity_tokens)
        .or_else(|| session_fill.and_then(|fill| window(fill.capacity_tokens)));
    let mut rows = vec![
        ContextRow::Fill(fill_text(breakdown.fill, capacity)),
        ContextRow::Blank,
    ];
    for part in breakdown.parts.iter().filter(|part| part.tokens > 0) {
        rows.push(ContextRow::Part {
            label: source_label(&part.source).to_owned(),
            tokens: compact_count(part.tokens),
            share: capacity.map(|capacity| share(part.tokens, capacity)),
        });
        rows.extend(item_rows(&part.items));
    }
    if let Some(capacity) = capacity {
        rows.push(ContextRow::Blank);
        let reserved = breakdown.reserved_tokens.unwrap_or(0);
        if reserved > 0 {
            rows.push(ContextRow::Capacity {
                label: "Reserved",
                tokens: compact_count(reserved),
                share: share(reserved, capacity),
            });
        }
        let free = capacity
            .saturating_sub(breakdown.fill.occupied_tokens)
            .saturating_sub(reserved);
        rows.push(ContextRow::Capacity {
            label: "Free",
            tokens: compact_count(free),
            share: share(free, capacity),
        });
    }
    rows
}

fn item_rows(items: &[ContextItem]) -> Vec<ContextRow> {
    let mut items = items
        .iter()
        .filter(|item| item.tokens > 0)
        .collect::<Vec<_>>();
    items.sort_by_key(|item| std::cmp::Reverse(item.tokens));
    let rest = items.split_off(items.len().min(NAMED_ITEMS));
    let mut rows = items
        .into_iter()
        .map(|item| ContextRow::Item {
            label: item.label.clone(),
            tokens: compact_count(item.tokens),
        })
        .collect::<Vec<_>>();
    if !rest.is_empty() {
        rows.push(ContextRow::Item {
            label: format!("{} more", rest.len()),
            tokens: compact_count(rest.iter().map(|item| item.tokens).sum()),
        });
    }
    rows
}

fn source_label(source: &ContextSource) -> &str {
    match source {
        ContextSource::SystemPrompt => "System prompt",
        ContextSource::SystemTools => "System tools",
        ContextSource::McpTools => "MCP tools",
        ContextSource::Instructions => "Instructions",
        ContextSource::Skills => "Skills",
        ContextSource::Agents => "Agents",
        ContextSource::Messages => "Messages",
        ContextSource::Other { label } => label,
    }
}

/// A window to measure against: zero is no window at all.
fn window(capacity: Option<u64>) -> Option<u64> {
    capacity.filter(|capacity| *capacity > 0)
}

fn fill_text(fill: ContextFill, capacity: Option<u64>) -> String {
    let occupied = compact_count(fill.occupied_tokens);
    match window(capacity) {
        Some(capacity) => format!(
            "{occupied} of {} tokens ({})",
            compact_count(capacity),
            share(fill.occupied_tokens, capacity)
        ),
        None => format!("{occupied} tokens"),
    }
}

/// `tokens` as a whole-number percentage of `capacity`, rounding halves up
/// as the footer's Context Fill does, except that a share too small to round
/// to one percent still reads as more than nothing.
fn share(tokens: u64, capacity: u64) -> String {
    let rounded = (u128::from(tokens) * 100 + u128::from(capacity) / 2) / u128::from(capacity);
    if rounded == 0 && tokens > 0 {
        "<1%".to_owned()
    } else {
        format!("{rounded}%")
    }
}
