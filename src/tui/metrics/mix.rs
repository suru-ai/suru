//! The Usage mix strip the Tokens measure shows under its legend: one bar
//! divided among fresh input, cache reads, cache writes, and output, and a
//! key naming each share and how much of the output was reasoning.

use ratatui::{buffer::Buffer, layout::Rect};
use unicode_width::UnicodeWidthStr;

use crate::{protocol::UsageMetricsAggregate, theme::Theme};

use super::{super::slots::truncate_to_width, view::share};

/// The rows the Usage mix strip takes: its bar, and the key beneath it.
pub(super) const MIX_ROWS: u16 = 2;
/// What stands before the Usage mix strip's bar.
const MIX_LEAD: &str = "Usage mix ";
/// The Usage mix strip's bar runs no longer than this.
const MIX_BAR: usize = 64;

/// The parts the Usage mix strip divides its bar among, in its order, each
/// with the shade it is drawn in and its names in the key, long and short.
const MIX_PARTS: [(char, &str, &str); 4] = [
    ('█', "fresh input", "input"),
    ('▓', "cache reads", "cache read"),
    ('▒', "cache writes", "cache write"),
    ('░', "output", "output"),
];

/// The parts of `aggregate`'s Usage the strip shows, in [`MIX_PARTS`]
/// order: each one reported, however small, and none that was not.
fn mix_parts(aggregate: &UsageMetricsAggregate) -> [Option<u64>; 4] {
    [
        aggregate.fresh_input_tokens,
        aggregate.cache_read_tokens,
        aggregate.cache_write_tokens,
        aggregate.output_tokens,
    ]
}

/// Draws the Usage mix strip in the first [`MIX_ROWS`] of `area`: one bar
/// divided among fresh input, cache reads, cache writes, and output, each
/// as its share of them all, and a key beneath naming each share and how
/// much of the output was reasoning. A part never reported has no place in
/// either; where nothing was reported there is no strip.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    aggregate: &UsageMetricsAggregate,
    theme: &Theme,
) {
    let parts = mix_parts(aggregate);
    let whole: u64 = parts
        .iter()
        .flatten()
        .fold(0, |sum, part| sum.saturating_add(*part));
    if whole == 0 || area.height == 0 || area.is_empty() {
        return;
    }
    let lead_width = u16::try_from(MIX_LEAD.width()).unwrap_or(u16::MAX);
    buffer.set_string(area.x, area.y, MIX_LEAD, theme.text.subdued);
    let bar_width = usize::from(area.width.saturating_sub(lead_width)).min(MIX_BAR);
    let shade = theme.accent.primary;
    let cells = cells(&parts, whole, bar_width);
    let bar: String = MIX_PARTS
        .iter()
        .zip(&cells)
        .map(|((glyph, _, _), cells)| glyph.to_string().repeat(*cells))
        .collect();
    buffer.set_string(area.x + lead_width, area.y, bar, shade);
    if area.height < 2 {
        return;
    }
    let key = |short: bool| {
        let mut entries = Vec::new();
        for ((glyph, long, brief), part) in MIX_PARTS.iter().zip(parts) {
            let Some(part) = part else { continue };
            let name = if short { brief } else { long };
            let mut entry = format!("{glyph} {name} {}", share(part, whole));
            if *glyph == '░'
                && let Some(reasoning) = aggregate.reasoning_tokens
                && part > 0
            {
                let reasoning = share(reasoning.min(part), part);
                if short {
                    entry.push_str(&format!(" ({reasoning} reasoning)"));
                } else {
                    entry.push_str(&format!(", {reasoning} of it reasoning"));
                }
            }
            entries.push(entry);
        }
        entries.join("  ")
    };
    let room = usize::from(area.width);
    let long = key(false);
    let key = if long.width() <= room {
        long
    } else {
        key(true)
    };
    let key = truncate_to_width(&key, room);
    let y = area.y + 1;
    buffer.set_string(area.x, y, &key, theme.text.subdued);
    // The swatches take the bar's own shade.
    let mut x = area.x;
    for character in key.chars() {
        if MIX_PARTS.iter().any(|(glyph, _, _)| *glyph == character) {
            buffer.set_string(x, y, character.to_string(), shade);
        }
        x += u16::try_from(character.to_string().width()).unwrap_or(1);
    }
}

/// How many of `width` cells each of `parts` takes, in proportion to its
/// share of `whole`, by the largest remainder, so they fill the bar exactly
/// and every part with any tokens takes a cell where there are cells enough.
fn cells(parts: &[Option<u64>; 4], whole: u64, width: usize) -> [usize; 4] {
    let whole = u128::from(whole.max(1));
    let cells_wide = u128::try_from(width).unwrap_or(u128::MAX);
    let exact = parts.map(|part| u128::from(part.unwrap_or(0)).saturating_mul(cells_wide));
    let mut cells = exact.map(|exact| usize::try_from(exact / whole).unwrap_or(0));
    let mut left = width.saturating_sub(cells.iter().sum());
    let mut by_remainder: Vec<usize> = (0..parts.len()).collect();
    by_remainder.sort_by_key(|index| std::cmp::Reverse(exact[*index] % whole));
    for index in by_remainder {
        if left == 0 {
            break;
        }
        if exact[index] > 0 {
            cells[index] += 1;
            left -= 1;
        }
    }
    // A sliver still shows, taking its cell from the largest part.
    for index in 0..parts.len() {
        if parts[index].is_some_and(|part| part > 0) && cells[index] == 0 {
            let largest = (0..parts.len())
                .max_by_key(|other| cells[*other])
                .expect("four parts");
            if cells[largest] > 1 {
                cells[largest] -= 1;
                cells[index] = 1;
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::cells;

    #[test]
    fn the_mix_fills_its_bar_and_a_sliver_still_takes_a_cell() {
        assert_eq!(
            cells(&[Some(1), Some(1), None, Some(2)], 4, 8),
            [2, 2, 0, 4]
        );
        assert_eq!(
            cells(&[Some(1), Some(10_000), None, Some(0)], 10_001, 10),
            [1, 9, 0, 0]
        );
    }
}
