//! The Insights tab in a small terminal: below the mockup's width its panels
//! stack in one column, and wherever they run past the foot of the frame
//! they scroll, so every panel can be reached.

use super::{READING_ORDER, on_insights, press_tab, worked, workspaces};
use crate::tui_rendering::{
    metrics::{press_at, wheel},
    support::{
        invoke, key, rendered_application_buffer, rendered_application_rows_at, text_position,
    },
};
use crossterm::event::{KeyCode, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    style::{Modifier, Style},
};
use suru::tui::{Application, ApplicationTransition, InsightsPanel, SemanticCommandId};

/// The narrowest frame the view is drawn in.
const NARROW: u16 = 80;
/// The shortest frame the view is drawn in.
const SHORT: u16 = 24;
/// A frame tall enough for every panel stacked in one column.
const TALL: u16 = 120;

/// The corners of the panel titled `title` in `buffer`, top-left and
/// bottom-right, where it is drawn whole.
fn panel_box(buffer: &Buffer, title: &str) -> ((u16, u16), (u16, u16)) {
    let (x, y) = text_position(buffer, title);
    let symbol = |column: u16, row: u16| buffer[(column, row)].symbol().to_owned();
    let left = (0..x)
        .rev()
        .find(|column| symbol(*column, y) == "╭")
        .unwrap_or_else(|| panic!("no corner left of {title:?}"));
    let right = (x..buffer.area.width)
        .find(|column| symbol(*column, y) == "╮")
        .unwrap_or_else(|| panic!("no corner right of {title:?}"));
    let bottom = (y + 1..buffer.area.height)
        .find(|row| symbol(left, *row) == "╰")
        .unwrap_or_else(|| panic!("no foot beneath {title:?}"));
    ((left, y), (right, bottom))
}

fn shows(application: &Application, width: u16, height: u16, needle: &str) -> bool {
    rendered_application_rows_at(application, width, height)
        .iter()
        .any(|row| row.contains(needle))
}

/// Whether the panel titled `title` stands whole in the frame.
fn shows_whole(application: &Application, width: u16, height: u16, title: &str) -> bool {
    let buffer = rendered_application_buffer(application, width, height);
    let rows = rendered_application_rows_at(application, width, height);
    rows.iter().any(|row| row.contains(title)) && {
        let (x, y) = text_position(&buffer, title);
        let left = (0..x)
            .rev()
            .find(|column| buffer[(*column, y)].symbol() == "╭");
        left.is_some_and(|left| {
            (y + 1..buffer.area.height).any(|row| buffer[(left, row)].symbol() == "╰")
        })
    }
}

fn border_at(application: &Application, width: u16, height: u16, title: &str) -> Style {
    let buffer = rendered_application_buffer(application, width, height);
    let ((x, y), _) = panel_box(&buffer, title);
    buffer[(x, y)].style()
}

#[test]
fn at_80_columns_every_panel_stacks_in_one_column_with_nothing_overlapping() {
    let application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, NARROW, TALL);
    let boxes: Vec<_> = READING_ORDER
        .iter()
        .map(|title| panel_box(&buffer, title))
        .collect();

    for (((left, _), (right, _)), title) in boxes.iter().zip(READING_ORDER) {
        assert_eq!(
            (*left, *right),
            (boxes[0].0.0, boxes[0].1.0),
            "{title:?} stands as wide as the one column"
        );
    }
    for (pair, titles) in boxes.windows(2).zip(READING_ORDER.windows(2)) {
        let (above, below) = (pair[0], pair[1]);
        assert!(
            above.1.1 < below.0.1,
            "{:?} ends above {:?} begins: {above:?} {below:?}",
            titles[0],
            titles[1]
        );
    }
    let rows = rendered_application_rows_at(&application, NARROW, TALL);
    assert!(
        rows.iter()
            .any(|row| row.contains("00      04      08      12      16      20")),
        "a column as wide as the frame gives When you work two columns an hour:\n{}",
        rows.join("\n")
    );
}

#[test]
fn page_down_and_page_up_scroll_the_column_until_every_panel_has_been_shown() {
    assert_eq!(
        SemanticCommandId::MetricsInsightsPageDown.as_str(),
        "metrics.insights.page-down"
    );
    assert_eq!(
        SemanticCommandId::MetricsInsightsPageUp.as_str(),
        "metrics.insights.page-up"
    );
    let mut application = on_insights(Ok(worked()));
    assert!(!shows(&application, NARROW, SHORT, " Delegation "));

    let mut seen: Vec<&str> = Vec::new();
    for _ in 0..20 {
        seen.extend(
            READING_ORDER
                .iter()
                .filter(|title| shows_whole(&application, NARROW, SHORT, title)),
        );
        assert_eq!(
            key(&mut application, KeyCode::PageDown),
            ApplicationTransition::Continue
        );
    }
    for title in READING_ORDER {
        assert!(seen.contains(&title), "{title:?} was shown whole");
    }
    assert!(
        shows_whole(&application, NARROW, SHORT, " Delegation "),
        "the column stops at its foot, the last panel in view"
    );

    for _ in 0..20 {
        key(&mut application, KeyCode::PageUp);
    }
    assert!(shows_whole(&application, NARROW, SHORT, "When you work"));
}

#[test]
fn focusing_a_panel_scrolls_it_into_view() {
    let mut application = on_insights(Ok(worked()));
    rendered_application_buffer(&application, NARROW, SHORT);
    let unfocused = border_at(&application, NARROW, SHORT, "When you work");

    key(&mut application, KeyCode::BackTab);
    assert!(shows_whole(&application, NARROW, SHORT, " Delegation "));
    assert_ne!(
        border_at(&application, NARROW, SHORT, " Delegation "),
        unfocused,
        "the last panel is focused, and in view"
    );

    press_tab(&mut application);
    assert!(
        shows_whole(&application, NARROW, SHORT, "When you work"),
        "focus turning round to the first scrolls back up to it"
    );

    invoke(
        &mut application,
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Approvals),
    );
    assert!(shows_whole(&application, NARROW, SHORT, " Approvals "));
}

#[test]
fn in_a_wide_but_short_frame_the_grid_scrolls_too_so_no_panel_is_lost() {
    let mut application = on_insights(Ok(worked()));
    assert!(!shows_whole(&application, 120, SHORT, " Delegation "));

    for _ in 0..3 {
        key(&mut application, KeyCode::PageDown);
    }

    assert!(shows_whole(&application, 120, SHORT, " Delegation "));
    assert!(shows_whole(&application, 120, SHORT, " Approvals "));
}

#[test]
fn a_frame_too_small_for_the_view_shows_only_the_message_on_insights_too() {
    let mut application = on_insights(Ok(worked()));
    let before = rendered_application_rows_at(&application, NARROW, TALL);
    for (width, height) in [(NARROW - 1, 40), (NARROW, SHORT - 1)] {
        let rows = rendered_application_rows_at(&application, width, height);
        let shown: Vec<&str> = rows
            .iter()
            .map(|row| row.trim())
            .filter(|row| !row.is_empty())
            .collect();
        assert!(
            shown
                .first()
                .is_some_and(|row| row.starts_with("Make the terminal larger")),
            "{width}×{height}: {shown:#?}"
        );
        assert!(shown.len() <= 2, "the message alone: {shown:#?}");

        // Nothing drawn, so nothing to focus, select, scroll, or click.
        key(&mut application, KeyCode::PageDown);
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(3) {
                press_at(&mut application, (x, y));
                wheel(&mut application, MouseEventKind::ScrollDown, (x, y));
            }
        }
    }
    assert_eq!(
        rendered_application_rows_at(&application, NARROW, TALL),
        before,
        "nothing focused, selected, scrolled, or opened"
    );
}

/// Where the top-left corner of the panel titled `title` stands in a frame
/// `width` × `height`, where its title is shown.
fn corner(application: &Application, width: u16, height: u16, title: &str) -> (u16, u16) {
    panel_box(
        &rendered_application_buffer(application, width, height),
        title,
    )
    .0
}

/// Pages down the narrowest, shortest frame until the panel titled `title`
/// stands whole in it.
fn page_to(application: &mut Application, title: &str) {
    for _ in 0..10 {
        if shows_whole(application, NARROW, SHORT, title) {
            return;
        }
        key(application, KeyCode::PageDown);
    }
    panic!("{title:?} never stood whole");
}

#[test]
fn the_wheel_scrolls_the_column_except_over_rows_of_a_panel_s_own_until_they_end() {
    assert_eq!(
        SemanticCommandId::MetricsInsightsScrollDown.as_str(),
        "metrics.insights.scroll.down"
    );
    assert_eq!(
        SemanticCommandId::MetricsInsightsScrollUp.as_str(),
        "metrics.insights.scroll.up"
    );
    let mut application = on_insights(Ok(worked()));
    let over_heatmap = text_position(
        &rendered_application_buffer(&application, NARROW, SHORT),
        "Wed",
    );
    let (_, top) = corner(&application, NARROW, SHORT, "When you work");

    assert_eq!(
        wheel(&mut application, MouseEventKind::ScrollDown, over_heatmap),
        ApplicationTransition::Continue
    );
    let buffer = rendered_application_buffer(&application, NARROW, SHORT);
    assert_eq!(
        text_position(&buffer, "Wed").1,
        over_heatmap.1 - 3,
        "a wheel's step scrolls the column three rows"
    );
    assert!(!shows(&application, NARROW, SHORT, "When you work"));
    wheel(&mut application, MouseEventKind::ScrollUp, over_heatmap);
    assert_eq!(corner(&application, NARROW, SHORT, "When you work").1, top);

    page_to(&mut application, "Tools used");
    let tools = corner(&application, NARROW, SHORT, "Tools used");
    let over_tools = (tools.0 + 4, tools.1 + 3);
    let first_rows = panel_rows(&application, "Tools used");
    wheel(&mut application, MouseEventKind::ScrollDown, over_tools);
    assert_eq!(
        corner(&application, NARROW, SHORT, "Tools used"),
        tools,
        "over the Tools, the wheel scrolls their rows, the column staying put"
    );
    assert_ne!(panel_rows(&application, "Tools used"), first_rows);
    for _ in 0..2 {
        wheel(&mut application, MouseEventKind::ScrollDown, over_tools);
    }
    let last_rows = panel_rows(&application, "Tools used");
    assert_eq!(corner(&application, NARROW, SHORT, "Tools used"), tools);
    assert_eq!(last_rows.last().map(String::as_str), Some("WebFetch 3 1"));

    wheel(&mut application, MouseEventKind::ScrollDown, over_tools);
    let moved = corner(&application, NARROW, SHORT, "Tools used");
    assert!(
        moved.1 < tools.1,
        "at the Tools' last row the wheel carries on down the column: {moved:?}"
    );
    assert_eq!(panel_rows(&application, "Tools used"), last_rows);
}

/// The rows of the panel titled `title` in the narrowest, shortest frame,
/// inside its border, runs of spaces closed up.
fn panel_rows(application: &Application, title: &str) -> Vec<String> {
    let buffer = rendered_application_buffer(application, NARROW, SHORT);
    let ((left, top), (right, bottom)) = panel_box(&buffer, title);
    (top + 1..bottom)
        .map(|row| {
            (left + 1..right)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn the_wheel_between_panels_scrolls_the_column_too() {
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, NARROW, SHORT);
    let (_, (_, bottom)) = panel_box(&buffer, "When you work");
    let between = (10, bottom + 1);
    let (_, turns) = corner(&application, NARROW, SHORT, " Turns ");

    wheel(&mut application, MouseEventKind::ScrollDown, between);

    assert_eq!(corner(&application, NARROW, SHORT, " Turns ").1, turns - 3);
}

#[test]
fn clicks_land_on_the_cells_bins_and_rows_shown_in_the_scrolled_column() {
    // Three rows down, the heatmap shows Tuesday where Monday stood.
    let mut cell = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&cell, NARROW, SHORT);
    let (x, y) = text_position(&buffer, "Mon  ");
    wheel(&mut cell, MouseEventKind::ScrollDown, (x, y));
    let buffer = rendered_application_buffer(&cell, NARROW, SHORT);
    let (x, tuesday) = text_position(&buffer, "Tue  ");
    assert_eq!(tuesday, y - 2, "scrolled three rows, Monday among them");
    press_at(&mut cell, (x + 5 + 14 * 2, tuesday));
    assert!(
        shows(
            &cell,
            NARROW,
            SHORT,
            "Tue 14:00–15:00 · 40 Turns · 2.1M tokens · $14.20"
        ),
        "{}",
        rendered_application_rows_at(&cell, NARROW, SHORT).join("\n")
    );
    assert!(
        shows_whole(&cell, NARROW, SHORT, "When you work"),
        "the panel clicked into focus scrolls whole into view"
    );

    let mut bin = on_insights(Ok(worked()));
    page_to(&mut bin, " Turns ");
    let buffer = rendered_application_buffer(&bin, NARROW, SHORT);
    let (x, y) = text_position(&buffer, "▁▃▇█▆▃▂▁");
    press_at(&mut bin, (x + 3, y));
    assert!(
        shows(&bin, NARROW, SHORT, "1–2m · 80 Turns · 26%"),
        "the bin clicked is the one selected, named in full in a column this wide"
    );

    let mut tool = on_insights(Ok(worked()));
    page_to(&mut tool, "Tools used");
    let buffer = rendered_application_buffer(&tool, NARROW, SHORT);
    let (x, y) = text_position(&buffer, "Read");
    press_at(&mut tool, (x, y));
    let buffer = rendered_application_buffer(&tool, NARROW, SHORT);
    let (x, y) = text_position(&buffer, "Read");
    assert!(
        buffer[(x, y)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "the Tool clicked is the one selected"
    );

    let mut workspace = on_insights(Ok(worked()));
    page_to(&mut workspace, "Workspaces by Cost");
    let buffer = rendered_application_buffer(&workspace, NARROW, SHORT);
    let blog = text_position(&buffer, "blog");
    let opened = press_at(&mut workspace, blog);
    workspaces::answer_overview_by_workspace(&mut workspace, opened);
    assert_eq!(
        workspaces::dimmed(&workspace, ["suru", "relay-infra", "dotfiles", "blog"]),
        [Some(true), Some(true), Some(true), Some(false)],
        "the Workspace clicked is the one opened"
    );
}

#[test]
fn a_panel_scrolled_out_of_view_takes_no_click() {
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, NARROW, SHORT);
    let (x, y) = text_position(&buffer, "Mon  ");
    let unfocused = border_at(&application, NARROW, SHORT, "When you work");
    for _ in 0..4 {
        wheel(&mut application, MouseEventKind::ScrollDown, (x, y));
    }
    assert!(!shows(&application, NARROW, SHORT, "Mon  "));
    let (_, turns) = corner(&application, NARROW, SHORT, " Turns ");

    // Where the heatmap's Monday was drawn, the Turns panel now stands.
    press_at(&mut application, (x, turns + 1));

    assert_ne!(
        border_at(&application, NARROW, SHORT, " Turns "),
        unfocused,
        "the panel shown there is the one focused"
    );
}

#[test]
fn a_frame_that_grows_and_shrinks_again_or_a_new_range_scrolls_back_to_the_top() {
    let mut application = on_insights(Ok(worked()));
    rendered_application_buffer(&application, NARROW, SHORT);
    for _ in 0..5 {
        key(&mut application, KeyCode::PageDown);
    }
    assert!(!shows(&application, NARROW, SHORT, "When you work"));

    rendered_application_buffer(&application, NARROW, TALL);
    assert!(
        shows_whole(&application, NARROW, SHORT, "When you work"),
        "a frame tall enough for every panel settles the scroll at the top"
    );

    for _ in 0..5 {
        key(&mut application, KeyCode::PageDown);
    }
    key(&mut application, KeyCode::Char('['));
    key(&mut application, KeyCode::Char(']'));
    assert!(
        shows_whole(&application, NARROW, SHORT, "When you work"),
        "a new range scrolls back to the top"
    );
}
