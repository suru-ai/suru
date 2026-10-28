//! The When you work panel: the Turns placed in each hour of the reader's
//! week, a weekday × hour heatmap in their own time zone, shaded by how
//! many Turns each cell holds of the busiest's, a cell of which a reader
//! can select to read its Turns, tokens, and Cost.
//!
//! A cell takes one of five looks: a dot where it holds no Turn, otherwise
//! one of four shades, each its own glyph, so they read apart on a Theme of
//! few colours or none.
//!
//! The report gives Turns by UTC quarter-hour. Every UTC offset in use is a
//! whole number of quarter-hours, so each quarter-hour falls wholly within
//! one local hour, and the fold is exact. Which hour that is depends on the
//! offset in force at the quarter-hour, daylight saving included, so each is
//! placed by its own instant rather than by one offset for the range.

use jiff::{Timestamp, tz::TimeZone};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{Cost, QuarterHourTurns},
    theme::Theme,
};

use super::{
    super::{super::slots::truncate_to_width, calendar::MetricsRange, chart::MetricsMeasure},
    InsightsPanel, InsightsView,
    turns::counted,
};

/// Days in a week, Monday first.
pub(super) const DAYS: usize = 7;

/// Hours in a day.
pub(super) const HOURS: usize = 24;

/// The rows the panel draws inside its border: the hours above the grid,
/// a row for each day, and the line beneath naming the selected cell.
pub(super) const ROWS: u16 = 2 + DAYS as u16;

/// Each day's name, Monday's first.
const DAY_NAMES: [&str; DAYS] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The columns of a day's name and the gap after it, before its hours.
const LABEL_WIDTH: u16 = 5;

/// What the panel says in place of a week too wide for it.
const TOO_NARROW: &str = "Too narrow to show the week";

/// The glyph of each shade a cell holding Turns takes, the lightest first:
/// each a denser block than the one before it, and none the empty cell's
/// dot.
const SHADES: [&str; 4] = ["░", "▒", "▓", "█"];

/// The Turns placed in one local hour of the week, and what they consumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct HourCell {
    pub(super) turns: u64,
    /// Absent where none of them reported tokens.
    pub(super) tokens: Option<u64>,
    /// Absent where none of them contributes a known amount.
    pub(super) cost: Option<Cost>,
    pub(super) is_cost_partial: bool,
}

impl HourCell {
    fn add(&mut self, quarter: &QuarterHourTurns) {
        self.turns += quarter.turns;
        if let Some(tokens) = quarter.tokens {
            self.tokens = Some(self.tokens.unwrap_or(0).saturating_add(tokens));
        }
        if let Some(cost) = quarter.cost {
            self.cost = Some(self.cost.map_or(cost, |held| held.saturating_add(cost)));
        }
        self.is_cost_partial |= quarter.is_cost_partial;
    }
}

/// The week's hours, Monday's first, each holding the Turns placed in it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Heatmap {
    cells: [[HourCell; HOURS]; DAYS],
}

impl Heatmap {
    /// Folds `quarter_hours` into the local hours of `zone`'s week.
    pub(super) fn fold(quarter_hours: &[QuarterHourTurns], zone: &TimeZone) -> Self {
        let mut heatmap = Self::default();
        for quarter in quarter_hours {
            let Some((day, hour)) = local_hour(quarter, zone) else {
                continue;
            };
            heatmap.cells[day][hour].add(quarter);
        }
        heatmap
    }

    /// The cell of `day`, counted from Monday, at `hour`.
    pub(super) fn cell(&self, day: usize, hour: usize) -> &HourCell {
        &self.cells[day][hour]
    }

    /// How many Turns the busiest cell holds.
    pub(super) fn busiest(&self) -> u64 {
        self.cells
            .iter()
            .flatten()
            .map(|cell| cell.turns)
            .max()
            .unwrap_or(0)
    }
}

/// How many cells the panel offers for selection: every hour of the week,
/// where any holds a Turn, and none where the range holds none.
pub(super) fn items(heatmap: &Heatmap) -> usize {
    if heatmap.busiest() > 0 {
        DAYS * HOURS
    } else {
        0
    }
}

/// The glyph of the shade of a cell holding `turns` of the busiest cell's
/// `busiest`: the share of [`SHADES`] rounded up, so a cell holding any
/// Turn shows the lightest, and only the busiest and those near it the
/// fullest. None for a cell holding none.
fn shade(turns: u64, busiest: u64) -> Option<&'static str> {
    if turns == 0 || busiest == 0 {
        return None;
    }
    let levels = SHADES.len() as u128;
    let level = (u128::from(turns) * levels).div_ceil(u128::from(busiest));
    let level = usize::try_from(level.clamp(1, levels)).ok()?;
    Some(SHADES[level - 1])
}

/// Draws the panel's grid in `area`, inside its border — the hours across
/// its top, a row for each day — marking the cell `selected` and naming its
/// figures on the line beneath. Each hour takes two columns where the panel
/// is wide enough, otherwise one.
pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    heatmap: &Heatmap,
    range: MetricsRange,
    selected: Option<usize>,
    view: &InsightsView,
    theme: &Theme,
) {
    let room = usize::from(area.width);
    let busiest = heatmap.busiest();
    if busiest == 0 {
        let note = format!("No Turns {}", range.within());
        buffer.set_string(
            area.x,
            area.y,
            truncate_to_width(&note, room),
            theme.text.subdued,
        );
        return;
    }
    let hours = HOURS as u16;
    let width = if area.width >= LABEL_WIDTH + 2 * hours {
        2
    } else {
        1
    };
    if area.width < LABEL_WIDTH + width * hours || area.height < ROWS {
        // No column is laid out this narrow, but were one, it would say why
        // it stands empty rather than seem broken.
        buffer.set_string(
            area.x,
            area.y,
            truncate_to_width(TOO_NARROW, room),
            theme.text.subdued,
        );
        return;
    }
    let grid_x = area.x + LABEL_WIDTH;
    // An hour named every eight columns: every fourth hour at two columns
    // an hour, every sixth at one.
    let named_every = if width == 2 { 4 } else { 6 };
    for hour in (0..hours).step_by(named_every) {
        let x = grid_x + hour * width;
        buffer.set_string(x, area.y, format!("{hour:02}"), theme.text.subdued);
    }
    let accent = theme.accent.primary;
    for (day, name) in DAY_NAMES.iter().enumerate() {
        let y = area.y + 1 + day as u16;
        buffer.set_string(area.x, y, name, theme.text.subdued);
        for hour in 0..HOURS {
            let (glyph, style) = match shade(heatmap.cell(day, hour).turns, busiest) {
                Some(shade) => (shade.repeat(usize::from(width)), accent),
                None => {
                    let dot = if width == 2 { "· " } else { "·" };
                    (dot.to_owned(), theme.border.subdued)
                }
            };
            let style = if selected == Some(day * HOURS + hour) {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };
            buffer.set_string(grid_x + hour as u16 * width, y, glyph, style);
        }
    }
    view.record_item_targets(
        InsightsPanel::WhenYouWork,
        Rect::new(grid_x, area.y + 1, width * hours, DAYS as u16),
        width,
        0,
    );
    let y = area.y + 1 + DAYS as u16;
    match selected {
        Some(item) => {
            let named = named_cell(heatmap, item / HOURS, item % HOURS);
            buffer.set_string(
                area.x,
                y,
                truncate_to_width(&named, room),
                theme.text.primary,
            );
        }
        None => render_legend(buffer, Rect::new(area.x, y, area.width, 1), theme),
    }
}

/// The selected cell's weekday and hour, and its Turns, tokens, and Cost,
/// with a `+` where that Cost says less than they cost.
fn named_cell(heatmap: &Heatmap, day: usize, hour: usize) -> String {
    let cell = heatmap.cell(day, hour);
    let mut parts = vec![format!(
        "{} {hour:02}:00–{:02}:00",
        DAY_NAMES[day],
        hour + 1
    )];
    if cell.turns == 0 {
        parts.push("no Turns".to_owned());
        return parts.join(" · ");
    }
    parts.push(counted(cell.turns));
    if let Some(tokens) = cell.tokens {
        parts.push(MetricsMeasure::Tokens.total(tokens, false));
    }
    if cell.cost.is_some() || cell.is_cost_partial {
        let cost = cell.cost.map_or(0, Cost::nano_usd);
        parts.push(MetricsMeasure::Cost.total(cost, cell.is_cost_partial));
    }
    parts.join(" · ")
}

/// The shades from the lightest to the fullest, between the words that say
/// which way they run.
fn render_legend(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let mut x = area.x;
    let pieces = std::iter::once(("fewer ", theme.text.subdued))
        .chain(SHADES.map(|shade| (shade, theme.accent.primary)))
        .chain(std::iter::once((" more", theme.text.subdued)));
    for (text, style) in pieces {
        let left = usize::from(area.right().saturating_sub(x));
        let text = truncate_to_width(text, left);
        buffer.set_string(x, area.y, &text, style);
        x += u16::try_from(text.width()).unwrap_or(u16::MAX);
    }
}

/// The weekday, counted from Monday, and the hour `quarter` begins in, in
/// `zone`; none for a moment the calendar cannot place.
fn local_hour(quarter: &QuarterHourTurns, zone: &TimeZone) -> Option<(usize, usize)> {
    let millisecond = i64::try_from(quarter.start.0).ok()?;
    let local = zone.to_datetime(Timestamp::from_millisecond(millisecond).ok()?);
    let day = usize::try_from(local.weekday().to_monday_zero_offset()).ok()?;
    let hour = usize::try_from(local.hour()).ok()?;
    Some((day, hour))
}

#[cfg(test)]
mod tests {
    use jiff::{civil::DateTime, civil::date, tz::TimeZone};

    use super::{Heatmap, HourCell};
    use crate::protocol::{Cost, QUARTER_HOUR_MS, QuarterHourTurns, SessionTimestamp};

    const MONDAY: usize = 0;
    const TUESDAY: usize = 1;
    const SUNDAY: usize = 6;

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap_or_else(|error| panic!("the zone {name}: {error}"))
    }

    /// One Turn of `tokens` tokens costing `nano_usd`, placed in the UTC
    /// quarter-hour beginning at `utc`.
    fn quarter(utc: DateTime, tokens: u64, nano_usd: u64) -> QuarterHourTurns {
        let zoned = utc.to_zoned(TimeZone::UTC).expect("a UTC reading");
        let start = u64::try_from(zoned.timestamp().as_millisecond()).unwrap();
        assert_eq!(start % QUARTER_HOUR_MS, 0, "{utc} begins a quarter-hour");
        QuarterHourTurns {
            start: SessionTimestamp(start),
            turns: 1,
            tokens: Some(tokens),
            cost: Some(Cost::from_nano_usd(nano_usd)),
            is_cost_partial: false,
        }
    }

    /// The cells holding Turns, by weekday and hour, Monday's first.
    fn held(heatmap: &Heatmap) -> Vec<(usize, usize, u64)> {
        (0..super::DAYS)
            .flat_map(|day| (0..super::HOURS).map(move |hour| (day, hour)))
            .filter_map(|(day, hour)| {
                let turns = heatmap.cell(day, hour).turns;
                (turns > 0).then_some((day, hour, turns))
            })
            .collect()
    }

    #[test]
    fn a_half_hour_offset_puts_each_quarter_hour_in_its_local_hour() {
        // Kolkata is 5:30 ahead of UTC all year.
        let heatmap = Heatmap::fold(
            &[
                // Monday 23:45 there.
                quarter(date(2026, 10, 26).at(18, 15, 0, 0), 1, 1),
                // Tuesday 00:00 there, and 00:45.
                quarter(date(2026, 10, 26).at(18, 30, 0, 0), 2, 2),
                quarter(date(2026, 10, 26).at(19, 15, 0, 0), 4, 4),
                // Tuesday 01:00 there.
                quarter(date(2026, 10, 26).at(19, 30, 0, 0), 8, 8),
            ],
            &zone("Asia/Kolkata"),
        );

        assert_eq!(
            held(&heatmap),
            vec![(MONDAY, 23, 1), (TUESDAY, 0, 2), (TUESDAY, 1, 1)]
        );
        assert_eq!(
            *heatmap.cell(TUESDAY, 0),
            HourCell {
                turns: 2,
                tokens: Some(6),
                cost: Some(Cost::from_nano_usd(6)),
                is_cost_partial: false,
            }
        );
    }

    #[test]
    fn a_quarter_hour_offset_puts_each_quarter_hour_in_its_local_hour() {
        // Kathmandu is 5:45 ahead of UTC all year.
        let heatmap = Heatmap::fold(
            &[
                // Sunday 23:45 there: the week's last hour.
                quarter(date(2026, 10, 25).at(18, 0, 0, 0), 1, 1),
                // Monday 00:00 there: the week's first.
                quarter(date(2026, 10, 25).at(18, 15, 0, 0), 1, 1),
                // Monday 00:45 there, and 01:00.
                quarter(date(2026, 10, 25).at(19, 0, 0, 0), 1, 1),
                quarter(date(2026, 10, 25).at(19, 15, 0, 0), 1, 1),
            ],
            &zone("Asia/Kathmandu"),
        );

        assert_eq!(
            held(&heatmap),
            vec![(MONDAY, 0, 2), (MONDAY, 1, 1), (SUNDAY, 23, 1)]
        );
    }

    #[test]
    fn the_spring_transition_skips_an_hour_and_the_offset_after_it_places_what_follows() {
        // New York springs from 2:00 EST to 3:00 EDT on Sunday 8 March 2026,
        // at 07:00 UTC.
        let heatmap = Heatmap::fold(
            &[
                // 01:45 EST.
                quarter(date(2026, 3, 8).at(6, 45, 0, 0), 1, 1),
                // 03:00 EDT, a quarter-hour later.
                quarter(date(2026, 3, 8).at(7, 0, 0, 0), 1, 1),
                // 04:00 EDT, where a fixed offset of -5 would read 03:00.
                quarter(date(2026, 3, 8).at(8, 0, 0, 0), 1, 1),
            ],
            &zone("America/New_York"),
        );

        assert_eq!(
            held(&heatmap),
            vec![(SUNDAY, 1, 1), (SUNDAY, 3, 1), (SUNDAY, 4, 1)]
        );
    }

    #[test]
    fn the_autumn_transition_repeats_an_hour_and_both_of_its_passes_land_in_it() {
        // New York falls back from 2:00 EDT to 1:00 EST on Sunday 1 November
        // 2026, at 06:00 UTC.
        let heatmap = Heatmap::fold(
            &[
                // 01:30 EDT, and 01:30 EST an hour later.
                quarter(date(2026, 11, 1).at(5, 30, 0, 0), 1, 1),
                quarter(date(2026, 11, 1).at(6, 30, 0, 0), 1, 1),
                // 02:00 EST.
                quarter(date(2026, 11, 1).at(7, 0, 0, 0), 1, 1),
                // 23:45 EDT the Saturday before.
                quarter(date(2026, 11, 1).at(3, 45, 0, 0), 1, 1),
            ],
            &zone("America/New_York"),
        );

        assert_eq!(
            held(&heatmap),
            vec![(5, 23, 1), (SUNDAY, 1, 2), (SUNDAY, 2, 1)]
        );
    }

    #[test]
    fn a_panel_too_narrow_for_the_week_says_so_and_offers_no_cell_to_click() {
        use ratatui::{buffer::Buffer, layout::Rect};

        let heatmap = Heatmap::fold(
            &[quarter(date(2026, 10, 26).at(9, 0, 0, 0), 10, 5)],
            &TimeZone::UTC,
        );
        let view = super::InsightsView::default();
        let area = Rect::new(0, 0, 28, super::ROWS);
        let mut buffer = Buffer::empty(area);

        super::render(
            &mut buffer,
            area,
            &heatmap,
            super::MetricsRange::ThirtyDays,
            None,
            &view,
            &crate::theme::Theme::system(),
        );

        let first: String = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect();
        assert_eq!(first.trim_end(), "Too narrow to show the week");
        assert_eq!(view.target_at(ratatui::layout::Position::new(10, 2)), None);
    }

    #[test]
    fn a_cell_s_cost_is_partial_where_any_of_its_quarter_hours_is() {
        let utc = TimeZone::UTC;
        let priced = quarter(date(2026, 10, 26).at(9, 0, 0, 0), 10, 5);
        let unpriced = QuarterHourTurns {
            start: SessionTimestamp(priced.start.0 + QUARTER_HOUR_MS),
            turns: 2,
            tokens: None,
            cost: None,
            is_cost_partial: true,
        };

        let heatmap = Heatmap::fold(&[priced, unpriced], &utc);

        assert_eq!(
            *heatmap.cell(MONDAY, 9),
            HourCell {
                turns: 3,
                tokens: Some(10),
                cost: Some(Cost::from_nano_usd(5)),
                is_cost_partial: true,
            }
        );
        assert_eq!(heatmap.busiest(), 3);
    }
}
