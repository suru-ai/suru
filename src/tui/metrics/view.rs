//! Draws the Usage Metrics view over the whole frame: a title bar with its
//! close control and the tab bar under it, then the tab showing. Overview
//! draws the range bar and the measures beside it, then the range's
//! headline figures, the chart, and its legend — with the Usage mix strip
//! under it for Tokens — and the Sessions that cost the most beneath them,
//! or what stands in for them while a read is on its way, when it was
//! refused, or when the range holds no Usage; Insights draws the range bar
//! alone and its panels. Where the frame is narrow the controls shorten and
//! the legend's series flow along a line; where it is smaller than the view
//! can be drawn in, it asks for a larger terminal.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{
        Outlook, UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsReport,
        UsageMetricsSession,
    },
    theme::Theme,
};

use super::{
    super::slots::truncate_to_width,
    BREAKDOWNS, BarStrip, FocusedRow, LEGEND_SERIES, MetricsContent, MetricsReadRefusal,
    MetricsTab, MetricsView, OfferedBreakdown,
    calendar::{MetricsRange, RangeBuckets},
    chart::{self, Figure, MetricsMeasure, Series},
    headline::{self, HEADLINE_ROWS},
    mix::{self, MIX_ROWS},
};

/// The close control, pointable as a whole.
const CLOSE: &str = "esc ✕";
const REFRESH: &str = "r refresh";
/// Columns the view's content stands in from either edge.
const INSET: u16 = 2;
/// The legend's proportional bars run no longer than this.
const LEGEND_BAR: u16 = 24;
/// Labels in the legend are cut to this many columns.
const LEGEND_LABEL: usize = 32;
/// Columns between a row of tabs and what follows it on the controls row.
const CONTROL_GAP: u16 = 4;
/// Columns between one tab's place in a row of them and the next's.
const TAB_GAP: u16 = 1;
/// What stands before the breakdowns on the controls row.
const BREAKDOWN_LEAD: &str = "by ";
/// What heads the Sessions that cost the most.
const TOP_SESSIONS_HEADING: &str = "Most expensive Sessions";
/// The fewest rows the chart keeps however many Sessions are listed under
/// it: the list takes room from the chart only down to this.
const CHART_FLOOR: u16 = 8;
/// The fewest rows the list is drawn in: a blank row, its heading, and one
/// Session, since a heading with no Session under it says nothing.
const LIST_FLOOR: u16 = 3;
/// Columns between a listed Session's figure, Title, Model, and Workspace.
const SESSION_GAPS: usize = 6;
/// Columns a listed Session's Title, Model, and Workspace are each cut to.
const SESSION_TITLE: usize = 40;
const SESSION_MODEL: usize = 24;
const SESSION_WORKSPACE: usize = 20;
/// Labels in a legend whose series flow along a line are cut to this many
/// columns.
const FLOWED_LABEL: usize = 24;
/// Columns between one series and the next along a line of them.
const FLOWED_GAP: usize = 2;
/// The fewest rows a legend whose series flow along a line is drawn in: its
/// heading and one line of them.
const FLOWED_LEGEND_FLOOR: u16 = 2;
/// The rows above the headline: the title bar, its rule, the tab bar, the
/// controls, and the row under them that says what became of a read.
const HEAD_ROWS: u16 = 5;
/// The narrowest frame the view is drawn in: the controls row stands whole
/// in 80 columns with the ranges in their short words and the widest
/// breakdown collapsed beside the measures, as do the headline's columns in
/// their short words; and every bar any range draws — no more than 57,
/// since no Turn is timed before 1970 — stands at least a column wide.
pub(super) const MIN_WIDTH: u16 = 80;
/// The shortest frame the view is drawn in: the rows above the headline,
/// the headline and the blank row under it, the chart at its floor, a blank
/// row, a legend flowing along one line, the Usage mix strip a blank row
/// under it, and the blank row the frame ends with — 24 rows.
pub(super) const MIN_HEIGHT: u16 =
    HEAD_ROWS + HEADLINE_ROWS + 1 + CHART_FLOOR + 1 + FLOWED_LEGEND_FLOOR + 1 + MIX_ROWS + 1;
/// What the view says in place of itself in a smaller frame.
const TOO_SMALL: &str = "Make the terminal larger to see Usage Metrics";
/// What stands in for the chart where All time's every Turn is untimed.
const UNCHARTED: &str =
    "This Usage was recorded before Suru timed Turns, so none of it can be charted.";

/// Draws `view` over the whole frame, the Outlook turned toward `outlook`,
/// whose figures alone it shows.
pub(in crate::tui) fn render(
    frame: &mut Frame<'_>,
    view: &MetricsView,
    outlook: &Outlook,
    theme: &Theme,
) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    view.forget_targets();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(buffer, area, view, theme);
        return;
    }
    let subdued = theme.text.subdued;
    let primary = theme.text.primary;

    buffer.set_string(
        area.x + 1,
        area.y,
        "Metrics",
        primary.add_modifier(Modifier::BOLD),
    );
    let close_width = u16::try_from(CLOSE.width()).unwrap_or(0);
    let close_x = area.right().saturating_sub(close_width + 1);
    buffer.set_string(close_x, area.y, CLOSE, subdued);
    view.record_close_target(Rect::new(close_x, area.y, close_width, 1));
    let refresh_x = close_x.saturating_sub(u16::try_from(REFRESH.width()).unwrap_or(0) + 3);
    buffer.set_string(refresh_x, area.y, REFRESH, subdued);
    for x in area.x + 1..area.right().saturating_sub(1) {
        buffer.set_string(x, area.y + 1, "─", theme.border.subdued);
    }

    render_tabs(
        buffer,
        area.x + INSET - 1,
        area.y + 2,
        MetricsTab::ALL.map(|tab| (tab.label(), tab == view.tab())),
        |place, at| view.record_tab_target(MetricsTab::ALL[place], at),
        theme,
    );
    if view.tab() == MetricsTab::Insights {
        render_insights(buffer, area, view, outlook, theme);
        return;
    }
    let range = view.range();
    let measure = view.measure();
    let breakdown = view.breakdown();
    let controls = Controls::fitting(area.width, breakdown);
    let controls_y = area.y + 3;
    let bar_end = render_tabs(
        buffer,
        area.x + INSET - 1,
        controls_y,
        MetricsRange::ALL.map(|chosen| (controls.range_label(chosen), chosen == range)),
        |place, at| view.record_range_target(MetricsRange::ALL[place], at),
        theme,
    );
    let measures_end = render_tabs(
        buffer,
        bar_end + CONTROL_GAP - 1,
        controls_y,
        MetricsMeasure::ALL.map(|chosen| (chosen.label(), chosen == measure)),
        |place, at| view.record_measure_target(MetricsMeasure::ALL[place], at),
        theme,
    );
    let lead_x = measures_end + CONTROL_GAP;
    buffer.set_string(lead_x, controls_y, BREAKDOWN_LEAD, subdued);
    let breakdowns_x = lead_x + u16::try_from(BREAKDOWN_LEAD.width()).unwrap_or(0);
    if controls.collapsed {
        // Collapsed to its choice, the control takes the next, as `b` does.
        render_tabs(
            buffer,
            breakdowns_x,
            controls_y,
            [(breakdown.label(), true)],
            |_, at| view.record_breakdown_target(breakdown.next(), at),
            theme,
        );
    } else {
        render_tabs(
            buffer,
            breakdowns_x,
            controls_y,
            BREAKDOWNS.map(|chosen| (chosen.label(), chosen == breakdown)),
            |place, at| view.record_breakdown_target(BREAKDOWNS[place], at),
            theme,
        );
    }
    // What is happening to the figures shown takes the row under the
    // controls, where it has the whole width to say it in.
    let status_y = controls_y + 1;
    let room = usize::from(area.width.saturating_sub(INSET * 2));
    if let Some(refusal) = view.stale_because() {
        // Figures that could not be read again stay, saying why above them.
        let note = truncate_to_width(&stale_note(refusal), room);
        buffer.set_string(area.x + INSET, status_y, note, theme.feedback.error);
    } else if view.is_reading() && matches!(view.content(outlook), MetricsContent::Report { .. }) {
        buffer.set_string(area.x + INSET, status_y, "Refreshing…", subdued);
    }

    let mut body = Rect::new(
        area.x + INSET,
        area.y + HEAD_ROWS,
        area.width.saturating_sub(INSET * 2),
        area.height.saturating_sub(HEAD_ROWS + 1),
    );
    // The headline states the range's figures above everything the report
    // draws, the hidden series among them: All time's count every Turn,
    // untimed ones too, where the chart can place only the timed.
    let mut headed = None;
    if let MetricsContent::Report { report, .. } = view.content(outlook) {
        let whole = if range == MetricsRange::AllTime {
            &report.across_buckets_and_untimed
        } else {
            &report.across_buckets.total
        };
        if headline::holds_anything(whole) && body.height > 0 {
            let rows = HEADLINE_ROWS.min(body.height);
            headline::render_headline(
                buffer,
                Rect::new(body.x, body.y, body.width, rows),
                whole,
                report.comparison.as_ref(),
                range,
                theme,
            );
            let taken = (HEADLINE_ROWS + 1).min(body.height);
            body = Rect::new(body.x, body.y + taken, body.width, body.height - taken);
            headed = Some(whole);
        }
    }
    if matches!(view.content(outlook), MetricsContent::Report { .. }) {
        // No legend or Session rows drawn until they say otherwise, so a
        // frame with no room for any leaves none to focus.
        view.record_legend_targets([None; LEGEND_SERIES]);
        view.record_session_targets(Rect::new(body.x, body.y, body.width, 0));
    }
    if body.is_empty() {
        return;
    }
    // The wheel steps ranges over where the chart stands, drawn or not, so a
    // range with nothing to draw can still be stepped past.
    view.record_chart_target(body);
    match view.content(outlook) {
        MetricsContent::Reading => {
            buffer.set_string(body.x, body.y, "Reading Usage Metrics…", subdued);
        }
        MetricsContent::Refused(refusal) => {
            let heading = match refusal {
                MetricsReadRefusal::Refused(_) => "The Server refused the Usage Metrics read",
                MetricsReadRefusal::Failed(_) => "Usage Metrics could not be read",
            };
            buffer.set_string(
                body.x,
                body.y,
                heading,
                theme.feedback.error.add_modifier(Modifier::BOLD),
            );
            let reason = truncate_to_width(refusal.reason(), usize::from(body.width));
            buffer.set_string(body.x, body.y + 1, reason, theme.feedback.error);
        }
        MetricsContent::Report { buckets, report } => {
            if chart::holds_nothing(report) {
                // Only All time's headline holds what its chart cannot: Turns
                // stored before Suru recorded when they ran.
                let uncharted = headed.filter(|_| range == MetricsRange::AllTime);
                let note = if uncharted.is_some() {
                    UNCHARTED
                } else {
                    range.no_usage()
                };
                let note = truncate_to_width(note, usize::from(body.width));
                buffer.set_string(body.x, body.y, note, subdued);
                // Under Tokens, the mix of what the headline states, a blank
                // row under the note, as under a legend.
                if let Some(whole) = uncharted
                    && measure == MetricsMeasure::Tokens
                {
                    let below = 2.min(body.height);
                    let mix = Rect::new(
                        body.x,
                        body.y + below,
                        body.width,
                        (body.height - below).min(MIX_ROWS),
                    );
                    mix::render(buffer, mix, whole, theme);
                }
                return;
            }
            let series = chart::series(report, measure, theme, view.kept_apart());
            // Hiding a series changes only the chart: the legend's heading,
            // figures, and shares stay those of every series, hidden or not,
            // as the headline figures above it will.
            let across = Legend::of_range(&series, measure, range, &report.across_buckets.total);
            // Room is kept for a note the range's legend or any bar's needs,
            // so selecting a bar never moves the chart.
            let noted = u16::from(
                across.noted
                    || (0..buckets.buckets.len()).any(|index| {
                        Legend::of_bucket(&series, measure, buckets, report, index)
                            .is_some_and(|legend| legend.noted)
                    }),
            );
            let mixed = measure == MetricsMeasure::Tokens;
            let mix_rows = if mixed { MIX_ROWS + 1 } else { 0 };
            // A row to each series where the chart keeps its floor beside
            // them; otherwise they flow along as few lines as hold them, and
            // as many as there is room for. They flow, too, wherever the
            // controls have collapsed: a row legend would fit there, but a
            // frame that narrow is the compact view, whose legend takes one
            // line so the chart keeps the rows.
            let in_rows = u16::try_from(series.len()).unwrap_or(u16::MAX);
            let flowed = (controls.collapsed
                || body.height < CHART_FLOOR + 1 + 1 + in_rows + noted + mix_rows)
                .then(|| Flowed::of(&series, measure, body.width));
            let series_rows = flowed.as_ref().map_or(in_rows, |flowed| {
                flowed.lines().min(
                    body.height
                        .saturating_sub(CHART_FLOOR + 1 + 1 + noted + mix_rows),
                )
            });
            let legend_rows = 1 + series_rows + noted + mix_rows;
            // The Sessions that cost the most stand a blank row under the
            // legend, headed, as many as there is room for down to the
            // chart's floor, and none at all below the list's own.
            let listed = u16::try_from(report.top_sessions.len()).unwrap_or(u16::MAX);
            let list_rows = if listed == 0 {
                0
            } else {
                listed.saturating_add(2).min(
                    body.height
                        .saturating_sub(legend_rows + 1)
                        .saturating_sub(CHART_FLOOR),
                )
            };
            let list_rows = if list_rows < LIST_FLOOR { 0 } else { list_rows };
            let chart_rows = body
                .height
                .saturating_sub(legend_rows + 1 + list_rows)
                .min(24);
            let chart_area = Rect::new(body.x, body.y, body.width, chart_rows);
            view.record_chart_target(chart_area);
            // Hidden series drop out of the stacks, so the axis rises only as
            // far as the series still shown reach.
            let stacked: Vec<Series> = series
                .iter()
                .filter(|series| !view.is_hidden(&series.id))
                .cloned()
                .collect();
            // Where the bars stand is recorded before anything is drawn, so
            // the selection the chart marks and the legend reads is one the
            // frame draws.
            let layout = chart::ChartLayout::of(chart_area, buckets, &stacked, measure);
            view.record_bar_targets(
                layout
                    .as_ref()
                    .and_then(chart::ChartLayout::bars)
                    .unwrap_or_else(|| no_bars(chart_area)),
            );
            let selected = view.drawn_selection();
            if let Some(layout) = &layout {
                chart::render_chart(buffer, layout, buckets, &stacked, selected, theme);
            }
            let legend = Rect::new(
                body.x,
                body.y + chart_rows + 1,
                body.width,
                body.height.saturating_sub(chart_rows + 1 + list_rows),
            );
            // The legend reads the bar the chart marks.
            let shown = selected
                .and_then(|index| Legend::of_bucket(&series, measure, buckets, report, index))
                .unwrap_or(across);
            let legend_area = Rect {
                height: legend.height.min(1 + series_rows + noted),
                ..legend
            };
            match &flowed {
                Some(flowed) => {
                    render_flowed_legend(frame, legend_area, &series, &shown, flowed, view, theme);
                }
                None => render_legend(frame, legend_area, &series, &shown, view, theme),
            }
            if mixed {
                // Under the legend, a blank row apart, reading what it reads.
                let after = (1 + series_rows + noted + 1).min(legend.height);
                let mix = Rect::new(
                    legend.x,
                    legend.y + after,
                    legend.width,
                    (legend.height - after).min(MIX_ROWS),
                );
                mix::render(frame.buffer_mut(), mix, &shown.aggregate, theme);
            }
            if list_rows > 0 {
                let list = Rect::new(body.x, legend.bottom() + 1, body.width, list_rows - 1);
                render_top_sessions(frame, list, &report.top_sessions, view, theme);
            }
        }
    }
}

/// What Insights says of All time where Turns stored before Suru timed them
/// lie outside it.
const UNTIMED_NOTE: &str = "Turns recorded before Suru timed Turns are not included";

/// The Insights tab beneath the tab bar: the range bar alone, the row under
/// it saying what became of a read — or of All time, what it leaves out —
/// and the panels.
fn render_insights(
    buffer: &mut Buffer,
    area: Rect,
    view: &MetricsView,
    outlook: &Outlook,
    theme: &Theme,
) {
    let range = view.range();
    let controls_y = area.y + 3;
    render_tabs(
        buffer,
        area.x + INSET - 1,
        controls_y,
        MetricsRange::ALL.map(|chosen| (chosen.label(), chosen == range)),
        |place, at| view.record_range_target(MetricsRange::ALL[place], at),
        theme,
    );
    let insights = view.insights();
    let room = usize::from(area.width.saturating_sub(INSET * 2));
    if let Some(refusal) = insights.stale_because(outlook, range) {
        let note = truncate_to_width(&stale_note(refusal), room);
        buffer.set_string(area.x + INSET, controls_y + 1, note, theme.feedback.error);
    } else if insights.is_reading(outlook, range)
        && matches!(
            insights.content(outlook, range),
            super::insights::InsightsContent::Report(_)
        )
    {
        buffer.set_string(
            area.x + INSET,
            controls_y + 1,
            "Refreshing…",
            theme.text.subdued,
        );
    } else if insights.leaves_out_untimed(outlook, range) {
        // Overview counts them in All time's headline; the panels, which
        // place every Turn in time, cannot.
        let note = truncate_to_width(UNTIMED_NOTE, room);
        buffer.set_string(area.x + INSET, controls_y + 1, note, theme.text.subdued);
    }
    let body = Rect::new(
        area.x + INSET,
        area.y + HEAD_ROWS,
        area.width.saturating_sub(INSET * 2),
        area.height.saturating_sub(HEAD_ROWS + 1),
    );
    super::insights::render(buffer, body, insights, outlook, range, theme);
}

/// What the view draws in a frame smaller than it can be drawn in: a request
/// for a larger terminal, centred, saying how large and how to leave, and
/// nothing a pointer or a key could act on.
fn render_too_small(buffer: &mut Buffer, area: Rect, view: &MetricsView, theme: &Theme) {
    view.record_bar_targets(no_bars(area));
    view.record_legend_targets([None; LEGEND_SERIES]);
    view.record_session_targets(Rect::new(area.x, area.y, 0, 0));
    let lines = [
        (TOO_SMALL.to_owned(), theme.text.primary),
        (
            format!(
                "It needs {MIN_WIDTH} × {MIN_HEIGHT} and has {} × {}. Esc closes.",
                area.width, area.height
            ),
            theme.text.subdued,
        ),
    ];
    let top = area.y + area.height.saturating_sub(2) / 2;
    for (row, (line, style)) in lines.iter().enumerate() {
        let Some(y) = u16::try_from(row)
            .ok()
            .map(|row| top + row)
            .filter(|y| *y < area.bottom())
        else {
            break;
        };
        let line = truncate_to_width(line, usize::from(area.width));
        let width = u16::try_from(line.width()).unwrap_or(area.width);
        let x = area.x + area.width.saturating_sub(width) / 2;
        buffer.set_string(x, y, line, *style);
    }
}

/// Where a frame drew no bar at all, at the corner of `area`: nothing to
/// point at or select.
fn no_bars(area: Rect) -> BarStrip {
    BarStrip {
        area: Rect::new(area.x, area.y, 0, 0),
        slot: 1,
        count: 0,
    }
}

/// How the controls row is drawn: the ranges in their long words or their
/// short, and every breakdown offered or the one chosen alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Controls {
    short_ranges: bool,
    collapsed: bool,
}

impl Controls {
    /// The fullest controls row that stands whole in a frame `width` wide
    /// with `breakdown` chosen: the ranges shortened first, then the
    /// breakdowns collapsed, and the narrowest where even that does not.
    fn fitting(width: u16, breakdown: UsageMetricsBreakdown) -> Self {
        let narrowest = Self {
            short_ranges: true,
            collapsed: true,
        };
        [
            Self {
                short_ranges: false,
                collapsed: false,
            },
            Self {
                short_ranges: true,
                collapsed: false,
            },
        ]
        .into_iter()
        .find(|controls| controls.width(breakdown) <= usize::from(width))
        .unwrap_or(narrowest)
    }

    fn range_label(self, range: MetricsRange) -> &'static str {
        if self.short_ranges {
            range.short_label()
        } else {
            range.label()
        }
    }

    /// The columns of a frame the row takes with `breakdown` chosen, the
    /// insets either side counted, as [`render`] lays it out.
    fn width(self, breakdown: UsageMetricsBreakdown) -> usize {
        let ranges = tabs_width(MetricsRange::ALL.map(|range| self.range_label(range)));
        let measures = tabs_width(MetricsMeasure::ALL.map(MetricsMeasure::label));
        let breakdowns = if self.collapsed {
            tabs_width([breakdown.label()])
        } else {
            tabs_width(BREAKDOWNS.map(OfferedBreakdown::label))
        };
        let inset = usize::from(INSET - 1);
        let gap = usize::from(CONTROL_GAP);
        inset + ranges + gap - 1 + measures + gap + BREAKDOWN_LEAD.width() + breakdowns + inset
    }
}

/// The columns [`render_tabs`] draws `labels` in.
fn tabs_width<const N: usize>(labels: [&str; N]) -> usize {
    let tabs: usize = labels.iter().map(|label| label.width() + 2).sum();
    tabs + usize::from(TAB_GAP) * N.saturating_sub(1)
}

/// The Sessions that cost the most under their heading, one to a row as
/// many as `area` has room for: each its Cost, with a `+` where it is
/// partial, its Title, the Model it cost the most under, and its Workspace,
/// in columns aligned down the list.
fn render_top_sessions(
    frame: &mut Frame<'_>,
    area: Rect,
    sessions: &[UsageMetricsSession],
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let drawn = u16::try_from(sessions.len())
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(1));
    view.record_session_targets(Rect::new(area.x, area.y + 1, area.width, drawn));
    if area.height == 0 {
        return;
    }
    buffer.set_string(
        area.x,
        area.y,
        TOP_SESSIONS_HEADING,
        theme.text.primary.add_modifier(Modifier::BOLD),
    );
    let sessions = &sessions[..usize::from(drawn)];
    let costs: Vec<String> = sessions
        .iter()
        .map(|session| {
            MetricsMeasure::Cost.figure(session.cost.cost.nano_usd(), session.cost.is_partial)
        })
        .collect();
    let widest = |width: &dyn Fn(&UsageMetricsSession) -> usize, most: usize| {
        sessions.iter().map(width).max().unwrap_or(0).min(most)
    };
    let cost_width = costs.iter().map(|cost| cost.width()).max().unwrap_or(0);
    let model_width = widest(
        &|session| session.model.as_deref().map_or(0, UnicodeWidthStr::width),
        SESSION_MODEL,
    );
    let workspace_width = widest(&|session| session.workspace.width(), SESSION_WORKSPACE);
    // The Title gives way first where the row is narrow, down to nothing
    // before the Model and Workspace are cut off the row's end.
    let title_width = widest(&|session| session.title.width(), SESSION_TITLE).min(
        usize::from(area.width)
            .saturating_sub(cost_width + model_width + workspace_width + SESSION_GAPS),
    );
    let focused = view.focused_row();
    for (place, (session, cost)) in sessions.iter().zip(&costs).enumerate() {
        let y = area.y + 1 + u16::try_from(place).unwrap_or(u16::MAX);
        let focused = focused == Some(FocusedRow::Session(place));
        let style = if focused {
            theme.selection.focused
        } else {
            theme.text.primary
        };
        if focused && let Some(x) = area.x.checked_sub(1) {
            // Marked in the inset before the figure, as a legend row is.
            buffer.set_string(x, y, "›", style);
        }
        let padded = |text: &str, width: usize| {
            let text = truncate_to_width(text, width);
            let padding = " ".repeat(width.saturating_sub(text.width()));
            format!("{text}{padding}")
        };
        let line = format!(
            "{cost:>cost_width$}  {}  {}  {}",
            padded(&session.title, title_width),
            padded(session.model.as_deref().unwrap_or(""), model_width),
            padded(&session.workspace, workspace_width),
        );
        let line = truncate_to_width(line.trim_end(), usize::from(area.width));
        buffer.set_string(area.x, y, line, style);
    }
}

/// Every tab from `x` along row `y`, each a label with a column either side
/// that a pointer can choose it by, recorded by its place in `tabs`, the
/// chosen one drawn as a filled tab in those columns, so no label moves as
/// the choice does. Answers the column after the last.
fn render_tabs<const N: usize>(
    buffer: &mut ratatui::buffer::Buffer,
    mut x: u16,
    y: u16,
    tabs: [(&str, bool); N],
    record: impl Fn(usize, Rect),
    theme: &Theme,
) -> u16 {
    let accent = theme.accent.primary;
    for (place, (label, chosen)) in tabs.into_iter().enumerate() {
        let label_width = u16::try_from(label.width()).unwrap_or(u16::MAX);
        if chosen {
            buffer.set_string(x, y, "▐", accent);
            buffer.set_string(
                x + 1,
                y,
                label,
                accent.add_modifier(Modifier::BOLD | Modifier::REVERSED),
            );
            buffer.set_string(x + 1 + label_width, y, "▌", accent);
        } else {
            buffer.set_string(x + 1, y, label, theme.text.subdued);
        }
        let width = label_width + 2;
        record(place, Rect::new(x, y, width, 1));
        x += width + TAB_GAP;
    }
    x - TAB_GAP
}

/// What the legend reads: the range's figures, or, where a bar is selected,
/// that bucket's in their place, so the legend is the bar's tooltip.
struct Legend {
    heading: String,
    /// Each series' figure, in the series' order.
    figures: Vec<Figure>,
    /// The whole each figure's share is of.
    whole: u64,
    /// Whether the figures count more than the whole, as Sessions under
    /// several keys do, so a note must say why.
    noted: bool,
    /// The figures of the whole in every measure, which the Usage mix strip
    /// reads.
    aggregate: UsageMetricsAggregate,
}

impl Legend {
    /// A legend of `figures` over `whole`, headed by `name` and that whole,
    /// then, where `counts`, the distinct Sessions that worked in it —
    /// never the series' Sessions added, which count a Session once under
    /// each key — unless the measure's whole already counts them, and its
    /// Turns.
    fn new(
        name: String,
        figures: Vec<Figure>,
        measure: MetricsMeasure,
        whole: &UsageMetricsAggregate,
        counts: bool,
    ) -> Self {
        let total = measure.value(whole);
        let mut heading = format!("{name} · {}", measure.total(measure.figure_of(whole)));
        if counts {
            if measure != MetricsMeasure::Sessions {
                heading.push_str(" · ");
                heading.push_str(&MetricsMeasure::Sessions.total(Figure::measured(whole.sessions)));
            }
            heading.push_str(" · ");
            heading.push_str(&chart::counted(whole.turns, "Turn", "Turns"));
        }
        Self {
            heading,
            noted: overcounts(&figures, measure, total),
            figures,
            whole: total,
            aggregate: *whole,
        }
    }

    /// The legend of the range taken together, headed by its name.
    fn of_range(
        series: &[Series],
        measure: MetricsMeasure,
        range: MetricsRange,
        whole: &UsageMetricsAggregate,
    ) -> Self {
        let figures = series.iter().map(|series| series.total).collect();
        Self::new(range.title().to_owned(), figures, measure, whole, false)
    }

    /// The legend of the `index`-th of `buckets` alone, headed by its local
    /// date or hour, its Sessions, and its Turns, where the report holds
    /// that bucket.
    fn of_bucket(
        series: &[Series],
        measure: MetricsMeasure,
        buckets: &RangeBuckets,
        report: &UsageMetricsReport,
        index: usize,
    ) -> Option<Self> {
        let bucket = buckets.buckets.get(index)?;
        let whole = &report.buckets.get(index)?.total;
        let figures = series
            .iter()
            .map(|series| series.figures.get(index).copied().unwrap_or_default())
            .collect();
        let name = chart::bucket_heading(bucket, buckets.unit);
        Some(Self::new(name, figures, measure, whole, true))
    }
}

/// The legend's heading, then one row per series in the chart's order —
/// ranked by Cost over the range whatever the measure or bucket, Other last —
/// each its colour, label, figure, share of the legend's whole, and a bar
/// proportional to the largest figure shown, a hidden series' row subdued.
/// Where the figures count more than the whole, a note under them says why.
fn render_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[Series],
    legend: &Legend,
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let measure = view.measure();
    let mut rows = [None; LEGEND_SERIES];
    for (row, place) in rows.iter_mut().take(series.len()).zip(1..area.height) {
        *row = Some(Rect::new(area.x, area.y + place, area.width, 1));
    }
    view.record_legend_targets(rows);
    if area.height == 0 {
        return;
    }
    let total = legend.whole;
    buffer.set_string(
        area.x,
        area.y,
        &legend.heading,
        theme.text.primary.add_modifier(Modifier::BOLD),
    );
    let label_width = series
        .iter()
        .map(|series| series.label.width())
        .max()
        .unwrap_or(0)
        .min(LEGEND_LABEL);
    let values: Vec<String> = legend
        .figures
        .iter()
        .map(|figure| measure.stated(*figure))
        .collect();
    let value_width = values.iter().map(|value| value.width()).max().unwrap_or(0);
    // Series stand in the same order whatever the measure, so the largest
    // in this one need not be the first.
    let largest = legend
        .figures
        .iter()
        .map(|figure| figure.value)
        .max()
        .unwrap_or(0)
        .max(1);
    let focused = view.focused_row();
    let fixed = 2 + label_width + 2 + value_width + 2 + 4 + 2;
    let bar_room = usize::from(area.width)
        .saturating_sub(fixed)
        .min(usize::from(LEGEND_BAR));
    for (row, ((series, value), figure)) in
        series.iter().zip(&values).zip(&legend.figures).enumerate()
    {
        // A figure that measured nothing, for want of any work, is no share
        // of the whole, as Errands are none of its Sessions.
        let share = if figure.is_measured || figure.is_partial {
            share(figure.value, total)
        } else {
            String::new()
        };
        let figure = &figure.value;
        let Some(y) = u16::try_from(row + 1)
            .ok()
            .map(|row| area.y + row)
            .filter(|y| *y < area.bottom())
        else {
            break;
        };
        // A hidden series keeps its row and figures, so it can be found and
        // shown again, its swatch and bar hollow and its words subdued: no
        // modifier a terminal may ignore says it alone.
        let hidden = view.is_hidden(&series.id);
        let glyph = if hidden { "░" } else { "█" };
        let swatch = Style::default().fg(series.color);
        buffer.set_string(area.x, y, glyph, swatch);
        let focused = focused == Some(FocusedRow::Series(row));
        let text_style = if focused {
            theme.selection.focused
        } else {
            theme.text.primary
        };
        let text_style = if hidden {
            text_style.patch(theme.text.subdued)
        } else {
            text_style
        };
        if focused && let Some(x) = area.x.checked_sub(1) {
            // Marked in the inset before the swatch, so no row moves.
            buffer.set_string(x, y, "›", text_style);
        }
        // Padded by the columns it is drawn in, not its characters, so a
        // wide glyph in one label leaves the figures beside every row aligned.
        let label = truncate_to_width(&series.label, label_width);
        let padding = " ".repeat(label_width.saturating_sub(label.width()));
        let text = format!("{label}{padding}  {value:>value_width$}  {share:>4}");
        buffer.set_string(area.x + 2, y, &text, text_style);
        let bar = usize::try_from(u128::from(*figure) * bar_room as u128 / u128::from(largest))
            .unwrap_or(0);
        let bar = if *figure > 0 { bar.max(1) } else { 0 };
        let bar_x = area.x + u16::try_from(fixed).unwrap_or(u16::MAX);
        if bar > 0 && bar_x < area.right() {
            buffer.set_string(bar_x, y, glyph.repeat(bar), swatch);
        }
    }
    let note_y = u16::try_from(series.len() + 1)
        .ok()
        .map(|row| area.y.saturating_add(row))
        .filter(|y| *y < area.bottom());
    if legend.noted
        && let Some(y) = note_y
    {
        let note = overlap_note(view.breakdown());
        let note = truncate_to_width(&note, usize::from(area.width));
        buffer.set_string(area.x, y, note, theme.text.subdued);
    }
}

/// Where the series of a legend flow along its lines: each at a column of
/// a line, in their order, as wide as its widest figure in any bucket or
/// across them all needs, so none moves as a bar is selected.
struct Flowed {
    /// Each series' place: its column from the legend's left edge, and its
    /// line from the first.
    places: Vec<(u16, u16)>,
    label_widths: Vec<usize>,
    value_widths: Vec<usize>,
}

impl Flowed {
    fn of(series: &[Series], measure: MetricsMeasure, room: u16) -> Self {
        let label_widths: Vec<usize> = series
            .iter()
            .map(|series| series.label.width().min(FLOWED_LABEL))
            .collect();
        let value_widths: Vec<usize> = series
            .iter()
            .map(|series| {
                series
                    .figures
                    .iter()
                    .chain([&series.total])
                    .map(|figure| measure.stated(*figure).width())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let room = usize::from(room);
        let (mut x, mut line) = (0, 0);
        let mut places = Vec::with_capacity(series.len());
        for (label, value) in label_widths.iter().zip(&value_widths) {
            let width = Self::item_width(*label, *value).min(room);
            if x > 0 && x + width > room {
                line += 1;
                x = 0;
            }
            places.push((
                u16::try_from(x).unwrap_or(u16::MAX),
                u16::try_from(line).unwrap_or(u16::MAX),
            ));
            x += width + FLOWED_GAP;
        }
        Self {
            places,
            label_widths,
            value_widths,
        }
    }

    /// A series' swatch and the column after it, then its label and its
    /// figure a column apart.
    fn item_width(label: usize, value: usize) -> usize {
        2 + label + 1 + value
    }

    /// How many lines the series flow along.
    fn lines(&self) -> u16 {
        self.places.last().map_or(0, |(_, line)| line + 1)
    }
}

/// The legend's heading, then its series flowing along the lines under it
/// as `flowed` places them, as many lines as `area` has room for, in the
/// chart's order: each its colour, label, and figure — its share is left to
/// a legend with a row to each — a hidden series' subdued. Where the figures
/// count more than the whole, a note under them says why.
fn render_flowed_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[Series],
    legend: &Legend,
    flowed: &Flowed,
    view: &MetricsView,
    theme: &Theme,
) {
    let buffer = frame.buffer_mut();
    let measure = view.measure();
    let lines = flowed
        .lines()
        .min(area.height.saturating_sub(1 + u16::from(legend.noted)));
    let mut items = [None; LEGEND_SERIES];
    if area.height == 0 {
        view.record_legend_targets(items);
        return;
    }
    buffer.set_string(
        area.x,
        area.y,
        &legend.heading,
        theme.text.primary.add_modifier(Modifier::BOLD),
    );
    let focused = view.focused_row();
    let room = usize::from(area.width);
    for (place, (((series, (x, line)), (label_width, value_width)), figure)) in series
        .iter()
        .zip(&flowed.places)
        .zip(flowed.label_widths.iter().zip(&flowed.value_widths))
        .zip(&legend.figures)
        .enumerate()
    {
        if *line >= lines {
            break;
        }
        let y = area.y + 1 + line;
        let at = area.x + x;
        let hidden = view.is_hidden(&series.id);
        let glyph = if hidden { "░" } else { "█" };
        buffer.set_string(at, y, glyph, Style::default().fg(series.color));
        let focused = focused == Some(FocusedRow::Series(place));
        let style = if focused {
            theme.selection.focused
        } else {
            theme.text.primary
        };
        let style = if hidden {
            style.patch(theme.text.subdued)
        } else {
            style
        };
        if focused && let Some(x) = at.checked_sub(1) {
            // Marked in the column before the swatch, so no series moves.
            buffer.set_string(x, y, "›", style);
        }
        let label = truncate_to_width(&series.label, *label_width);
        let padding = " ".repeat(label_width.saturating_sub(label.width()));
        let text = format!(
            "{label}{padding} {value:>value_width$}",
            value = measure.stated(*figure),
        );
        let left = room.saturating_sub(usize::from(*x) + 2);
        let text = truncate_to_width(&text, left);
        buffer.set_string(at + 2, y, &text, style);
        let width = u16::try_from(2 + text.width()).unwrap_or(u16::MAX);
        items[place] = Some(Rect::new(at, y, width, 1));
    }
    view.record_legend_targets(items);
    let note_y = area.y + 1 + lines;
    if legend.noted && note_y < area.bottom() {
        let note = overlap_note(view.breakdown());
        let note = truncate_to_width(&note, room);
        buffer.set_string(area.x, note_y, note, theme.text.subdued);
    }
}

/// Why the legend's series can count more Sessions than its heading does,
/// in the words of the breakdown that splits them.
fn overlap_note(breakdown: UsageMetricsBreakdown) -> String {
    format!(
        "A Session that used several {} counts once for each",
        breakdown.plural()
    )
}

/// Whether `figures` count more than `whole` does, as a measure that does
/// not add up can: a Session that worked under several keys counts once
/// under each and once in the whole.
fn overcounts(figures: &[Figure], measure: MetricsMeasure, whole: u64) -> bool {
    let counted = figures
        .iter()
        .map(|figure| figure.value)
        .fold(0, u64::saturating_add);
    !measure.adds_up() && counted > whole
}

/// What the view says of figures a refresh could not replace.
fn stale_note(refusal: &MetricsReadRefusal) -> String {
    match refusal {
        MetricsReadRefusal::Refused(reason) => format!("Not refreshed: {reason}"),
        MetricsReadRefusal::Failed(reason) => format!("Could not refresh: {reason}"),
    }
}

/// `part` of `whole` as a whole percentage, with a small part that rounds
/// to nothing still shown as there.
pub(super) fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return String::new();
    }
    let percent = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    if percent == 0 && part > 0 {
        "<1%".to_owned()
    } else {
        format!("{percent}%")
    }
}

#[cfg(test)]
mod tests {
    use super::share;

    #[test]
    fn a_share_is_a_whole_percentage_and_a_sliver_still_shows() {
        assert_eq!(share(58, 100), "58%");
        assert_eq!(share(2, 3), "67%");
        assert_eq!(share(1, 1_000), "<1%");
        assert_eq!(share(0, 1_000), "0%");
    }
}
