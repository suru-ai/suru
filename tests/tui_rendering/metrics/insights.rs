//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Outlook's Server worked over the range the Overview tab shares with it.

use super::{
    HEIGHT, NEW_YORK, WIDTH, afternoon, answer, ask, in_new_york, press, press_at, reader_in,
    report, row, screen, wheel, zone,
};
use crate::tui_rendering::support::{
    application_looking_at_studio, grace_elapses, invoke, key, rendered_application_buffer,
    studio_stops_answering, text_position,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use jiff::civil::date;
use ratatui::{buffer::Buffer, style::Style};
use suru::{
    protocol::{
        ApprovalInsights, CompactionCounts, CompactionInsights, Cost, FreedShare, InsightsQuery,
        InsightsReport, InsightsTool, Outlook, QUARTER_HOUR_MS, QuarterHourTurns, ToolTally,
        TurnDurations, TurnInsights, UsageMetricsInterval, UsageMetricsQuery,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, InsightsPanel, MetricsTab,
        SemanticCommandId, UsageMetricsReadRefusal,
    },
};
use uuid::Uuid;

fn ctrl(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
        .expect("deliver a key press")
}

fn command_for_ctrl(application: &Application, code: KeyCode) -> Option<CommandId> {
    application
        .command_for_terminal_input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
}

fn expect_insights_read(transition: ApplicationTransition) -> (Outlook, Uuid, InsightsQuery) {
    match transition {
        ApplicationTransition::ReadUsageInsights {
            origin,
            request_id,
            query,
        } => (origin, request_id, query),
        other => panic!("the tab reads the Outlook's Server's Insights, not {other:?}"),
    }
}

fn expect_metrics_read(transition: ApplicationTransition) -> UsageMetricsQuery {
    match transition {
        ApplicationTransition::ReadUsageMetrics { query, .. } => query,
        other => panic!("the Overview reads its Usage Metrics, not {other:?}"),
    }
}

fn answer_insights(
    application: &mut Application,
    request_id: Uuid,
    result: Result<InsightsReport, UsageMetricsReadRefusal>,
) {
    application
        .handle_event(ApplicationEvent::UsageInsightsRead { request_id, result })
        .expect("deliver the Insights");
}

/// The one interval a metrics query's buckets span, first to last.
fn spanned(query: &UsageMetricsQuery) -> UsageMetricsInterval {
    UsageMetricsInterval {
        start: query.buckets.first().expect("a bucket").start,
        end: query.buckets.last().expect("a bucket").end,
    }
}

/// Turns that took from under ten seconds to over half an hour, shaped as
/// the mockup's sparkline is: 310 timed, of 1,213 Completed, 40 Failed, and
/// 31 Interrupted.
fn worked() -> InsightsReport {
    InsightsReport {
        turns: TurnInsights {
            durations: Some(TurnDurations {
                median_ms: 48_000,
                p90_ms: 372_000,
                longest_ms: 2_465_000,
                bins: [10, 30, 70, 80, 60, 30, 20, 10],
            }),
            completed: 1_213,
            failed: 40,
            interrupted: 31,
        },
        quarter_hours: worked_hours(),
        tools: worked_tools(),
        compactions: worked_compactions(),
        approvals: worked_approvals(),
    }
}

/// The mockup's 27 Compactions: 6 manual, 1 failed, and on average 61% of
/// the context freed by the 26 that completed knowing both figures.
fn worked_compactions() -> CompactionInsights {
    CompactionInsights {
        automatic: CompactionCounts {
            active: 0,
            completed: 20,
            failed: 1,
            interrupted: 0,
        },
        manual: CompactionCounts {
            active: 0,
            completed: 6,
            failed: 0,
            interrupted: 0,
        },
        freed: Some(FreedShare {
            compactions: 26,
            average_millionths: 612_000,
        }),
    }
}

/// The mockup's 188 Approvals: 171 accepted, 12 declined, and 5 neither.
fn worked_approvals() -> ApprovalInsights {
    ApprovalInsights {
        asked: 188,
        accepted: 160,
        accepted_for_session: 11,
        declined: 10,
        declined_and_interrupted: 2,
        waiting: 1,
        expired: 4,
    }
}

fn tool(tool: InsightsTool, uses: u64, failed: u64) -> ToolTally {
    ToolTally { tool, uses, failed }
}

fn named(server: Option<&str>, name: &str) -> InsightsTool {
    InsightsTool::Named {
        name: name.to_owned(),
        server: server.map(str::to_owned),
    }
}

/// The Tools the mockup's reader's Agents used, the most used first as the
/// Server sends them: more than the panel has rows for.
fn worked_tools() -> Vec<ToolTally> {
    vec![
        tool(InsightsTool::Command, 4_210, 212),
        tool(InsightsTool::FileChange, 2_987, 9),
        tool(named(None, "Read"), 2_544, 3),
        tool(named(Some("github"), "create_pr"), 41, 2),
        tool(InsightsTool::Subagent, 30, 5),
        tool(named(None, "Grep"), 22, 0),
        tool(named(Some("fs"), "write"), 9, 4),
        tool(named(None, "WebFetch"), 3, 1),
    ]
}

/// `turns` Turns of `tokens` tokens costing `usd`, placed in the quarter-hour
/// beginning at `minute` past `hour` on `day` of October 2026 in New York.
fn new_york_quarter(
    day: i8,
    hour: i8,
    minute: i8,
    turns: u64,
    tokens: u64,
    usd: f64,
) -> QuarterHourTurns {
    let start = in_new_york(date(2026, 10, day).at(hour, minute, 0, 0));
    assert_eq!(start.0 % QUARTER_HOUR_MS, 0);
    QuarterHourTurns {
        start,
        turns,
        tokens: Some(tokens),
        cost: Cost::from_usd(usd),
        is_cost_partial: false,
    }
}

/// When the mockup's reader worked, in New York: the busiest hour Tuesday
/// 27 October at 14:00, its 40 Turns in two quarter-hours, and the hours
/// after it each quieter — 30, 20, then 10 Turns — so they shade a level
/// apiece less, and then 1 Turn, still the lightest shade rather than none;
/// and one Turn late on the Wednesday before, whose Cost is partial.
fn worked_hours() -> Vec<QuarterHourTurns> {
    let mut hours = vec![
        new_york_quarter(27, 14, 0, 25, 1_300_000, 9.20),
        new_york_quarter(27, 14, 45, 15, 800_000, 5.00),
        new_york_quarter(27, 15, 0, 30, 900_000, 3.00),
        new_york_quarter(27, 16, 30, 20, 600_000, 2.00),
        new_york_quarter(27, 17, 15, 10, 300_000, 1.00),
        new_york_quarter(27, 18, 0, 1, 30_000, 0.10),
    ];
    hours.push(QuarterHourTurns {
        is_cost_partial: true,
        ..new_york_quarter(21, 23, 45, 1, 500, 0.05)
    });
    hours
}

/// The view opened on 30 days, its Overview answered, then Insights shown,
/// asked, and answered with `insights`.
fn on_insights(insights: Result<InsightsReport, UsageMetricsReadRefusal>) -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let (_, request_id, _) = expect_insights_read(ctrl(&mut application, KeyCode::Right));
    answer_insights(&mut application, request_id, insights);
    application
}

#[test]
fn the_view_opens_on_overview_and_ctrl_right_reads_insights_for_the_shared_range() {
    assert_eq!(
        SemanticCommandId::MetricsTabNext.as_str(),
        "metrics.tab.next"
    );
    assert_eq!(
        SemanticCommandId::MetricsTabSelect(MetricsTab::Insights).as_str(),
        "metrics.tab.insights"
    );
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, overview) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&overview)));
    let rows = screen(&application);
    assert!(row(&rows, "Overview").contains("▐Overview▌"), "{rows:#?}");

    let (origin, _, query) = expect_insights_read(ctrl(&mut application, KeyCode::Right));

    assert_eq!(origin, Outlook::Local);
    assert_eq!(
        query.interval,
        spanned(&overview),
        "the thirty New York days the Overview reads, as one interval"
    );
    let rows = screen(&application);
    assert!(row(&rows, "Insights").contains("▐Insights▌"), "{rows:#?}");
    assert!(row(&rows, "30 days").contains("▐30 days▌"));
    row(&rows, "Reading Insights…");
}

#[test]
fn all_time_insights_reach_back_to_the_first_moment_a_turn_can_be_placed_at() {
    let mut application = on_insights(Ok(worked()));

    let (_, _, query) = expect_insights_read(press(&mut application, '5'));

    assert_eq!(query.interval.start.0, 0);
    assert!(query.interval.contains(afternoon()));
}

#[test]
fn switching_tabs_back_and_forth_reads_neither_again_and_r_reads_the_one_showing() {
    let mut application = on_insights(Ok(worked()));

    assert_eq!(
        ctrl(&mut application, KeyCode::Left),
        ApplicationTransition::Continue,
        "the Overview keeps its report"
    );
    assert!(
        screen(&application)
            .iter()
            .any(|row| row.contains("Claude Opus 5.5"))
    );
    assert_eq!(
        ctrl(&mut application, KeyCode::Right),
        ApplicationTransition::Continue,
        "Insights keeps its report for the range"
    );
    row(&screen(&application), "median 48s");

    expect_insights_read(press(&mut application, 'r'));
    row(&screen(&application), "median 48s");
    ctrl(&mut application, KeyCode::Left);
    expect_metrics_read(press(&mut application, 'r'));
}

#[test]
fn a_range_chosen_on_insights_is_the_overview_s_range_too() {
    let mut application = on_insights(Ok(worked()));

    let (_, request_id, seven_days) = expect_insights_read(press(&mut application, '2'));
    answer_insights(&mut application, request_id, Ok(InsightsReport::default()));
    let rows = screen(&application);
    assert!(row(&rows, "7 days").contains("▐7 days▌"), "{rows:#?}");

    let overview = expect_metrics_read(ctrl(&mut application, KeyCode::Left));
    assert_eq!(overview.buckets.len(), 7, "the Overview reads the 7 days");
    assert_eq!(spanned(&overview), seven_days.interval);
    let rows = screen(&application);
    assert!(row(&rows, "7 days").contains("▐7 days▌"), "{rows:#?}");

    // Back on 30 days, read on Insights already, Insights reads nothing.
    expect_metrics_read(press(&mut application, '3'));
    assert_eq!(
        ctrl(&mut application, KeyCode::Right),
        ApplicationTransition::Continue
    );
    row(&screen(&application), "median 48s");
}

#[test]
fn ctrl_arrows_and_clicks_on_the_tab_bar_switch_tabs_alike() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    assert_eq!(
        command_for_ctrl(&application, KeyCode::Right),
        Some(CommandId::InvokeSemantic(SemanticCommandId::MetricsTabNext))
    );
    assert_eq!(
        command_for_ctrl(&application, KeyCode::Left),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsTabPrevious
        ))
    );
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let insights_tab = text_position(&buffer, "Insights");
    let overview_tab = text_position(&buffer, "Overview");

    let (_, _, clicked) = expect_insights_read(press_at(&mut application, insights_tab));
    // The click took the view to Insights, as Ctrl+→ would have.
    let mut keyed = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut keyed);
    answer(&mut keyed, request_id, Ok(report(&query)));
    let (_, _, by_key) = expect_insights_read(ctrl(&mut keyed, KeyCode::Right));
    assert_eq!(clicked, by_key);
    assert_eq!(screen(&application), screen(&keyed));

    assert_eq!(
        press_at(&mut application, overview_tab),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application)
            .iter()
            .any(|row| row.contains("▐Overview▌"))
    );
    assert_eq!(
        invoke(
            &mut application,
            SemanticCommandId::MetricsTabSelect(MetricsTab::Insights)
        ),
        ApplicationTransition::Continue,
        "a read already on its way is not asked again"
    );
}

/// The style of the Turns panel's top-left border corner.
fn turns_border(application: &Application) -> Style {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, " Turns ");
    corner_left_of(&buffer, x, y)
}

fn corner_left_of(buffer: &Buffer, x: u16, y: u16) -> Style {
    let corner = (0..x)
        .rev()
        .find(|column| ["┌", "╭"].contains(&buffer[(*column, y)].symbol()))
        .expect("the panel's corner");
    buffer[(corner, y)].style()
}

#[test]
fn tab_or_a_click_focuses_a_panel_and_highlights_its_border() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Turns).as_str(),
        "metrics.panel.turns"
    );
    assert_eq!(
        SemanticCommandId::MetricsPanelItemSelect {
            panel: InsightsPanel::Turns,
            item: 0,
        }
        .as_str(),
        "metrics.panel.item.select"
    );
    let mut application = on_insights(Ok(worked()));
    let unfocused = turns_border(&application);

    assert_eq!(press_tab(&mut application), ApplicationTransition::Continue);
    assert_eq!(
        turns_border(&application),
        unfocused,
        "focus begins at the first panel"
    );
    press_tab(&mut application);
    let focused = turns_border(&application);
    assert_ne!(focused, unfocused, "the focused panel's border stands out");
    assert_eq!(
        key(&mut application, KeyCode::BackTab),
        ApplicationTransition::Continue
    );
    assert_eq!(turns_border(&application), unfocused);
    key(&mut application, KeyCode::BackTab);
    assert_eq!(
        (tools_border(&application), turns_border(&application)),
        (focused, unfocused),
        "from the first panel, focus moves round to the last"
    );

    let mut clicked = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&clicked, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, "✓");
    press_at(&mut clicked, (x, y));
    assert_eq!(turns_border(&clicked), focused);
}

fn press_tab(application: &mut Application) -> ApplicationTransition {
    key(application, KeyCode::Tab)
}

#[test]
fn esc_clears_the_selection_then_leaves_the_panel_then_closes_the_view() {
    let mut application = on_insights(Ok(worked()));
    let unfocused = turns_border(&application);
    press_tab(&mut application);
    press_tab(&mut application);
    let focused = turns_border(&application);
    key(&mut application, KeyCode::Right);
    key(&mut application, KeyCode::Right);
    row(&screen(&application), "10–30s · 30 Turns");

    key(&mut application, KeyCode::Esc);
    let rows = screen(&application);
    assert!(!rows.iter().any(|row| row.contains("10–30s · 30 Turns")));
    assert_eq!(turns_border(&application), focused, "the panel keeps focus");

    key(&mut application, KeyCode::Esc);
    assert_eq!(turns_border(&application), unfocused);
    row(&screen(&application), "median 48s");

    key(&mut application, KeyCode::Esc);
    assert!(
        !screen(&application)
            .iter()
            .any(|row| row.contains("median 48s")),
        "the view closed"
    );
}

#[test]
fn a_click_on_a_bin_selects_it_and_focuses_its_panel() {
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, "▁▃▇█▆▃▂▁");

    press_at(&mut application, (x + 3, y));

    row(&screen(&application), "1–2m · 80 Turns · 26%");
    assert_ne!(
        turns_border(&application),
        turns_border(&on_insights(Ok(worked())))
    );
}

#[test]
fn the_wheel_over_a_panel_is_the_panel_s_and_never_steps_the_range() {
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let over_turns = text_position(&buffer, "median 48s");

    for kind in [MouseEventKind::ScrollDown, MouseEventKind::ScrollUp] {
        assert_eq!(
            wheel(&mut application, kind, over_turns),
            ApplicationTransition::Continue
        );
    }
    let rows = screen(&application);
    assert!(row(&rows, "30 days").contains("▐30 days▌"));
}

#[test]
fn the_turns_panel_states_durations_their_spread_and_how_turns_ended() {
    let application = on_insights(Ok(worked()));

    let rows = screen(&application);

    row(&rows, "Turns");
    row(&rows, "median 48s · p90 6m 12s · longest 41m 5s");
    row(&rows, "▁▃▇█▆▃▂▁  <10s … 30m+");
    row(&rows, "✓ 1,213 completed  ✗ 40 failed  ⏹ 31 interrupted");
}

#[test]
fn a_range_with_no_turns_says_so_in_the_panel() {
    let application = on_insights(Ok(InsightsReport::default()));

    let rows = screen(&application);

    row(&rows, "Turns");
    row(&rows, "No Turns in the last 30 days");
}

#[test]
fn turns_that_ended_without_both_moments_are_counted_but_give_no_durations() {
    let application = on_insights(Ok(InsightsReport {
        turns: TurnInsights {
            durations: None,
            completed: 3,
            ..TurnInsights::default()
        },
        ..InsightsReport::default()
    }));

    let rows = screen(&application);

    row(&rows, "No Turn recorded how long it took");
    row(&rows, "✓ 3 completed  ✗ 0 failed  ⏹ 0 interrupted");
}

#[test]
fn a_refused_read_shows_its_reason_in_place_of_the_panels() {
    let application = on_insights(Err(UsageMetricsReadRefusal::Refused(
        "An Insights query's interval ends before it begins".to_owned(),
    )));

    let rows = screen(&application);

    row(&rows, "The Server refused the Insights read");
    row(&rows, "An Insights query's interval ends before it begins");
    assert!(!rows.iter().any(|row| row.contains("Turns")), "{rows:#?}");
}

#[test]
fn insights_asked_of_a_remote_that_stopped_answering_are_refused_in_the_tab() {
    let mut application = application_looking_at_studio()
        .with_session_clock(afternoon)
        .with_time_zone(zone(NEW_YORK));
    let (origin, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let (insights_origin, request_id, _) =
        expect_insights_read(ctrl(&mut application, KeyCode::Right));
    assert_eq!(insights_origin, origin);
    answer_insights(&mut application, request_id, Ok(worked()));
    studio_stops_answering(&mut application, 1, std::time::Duration::from_secs(5));
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));

    assert_eq!(
        press(&mut application, '2'),
        ApplicationTransition::Continue,
        "a range read waits for the Remote"
    );

    let rows = screen(&application);
    row(&rows, "studio is unreachable");
    assert!(row(&rows, "7 days").contains("▐7 days▌"), "{rows:#?}");
}

#[test]
fn g_and_i_switch_tabs_where_the_system_takes_ctrl_arrows() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));

    let (_, request_id, by_letter) = expect_insights_read(press(&mut application, 'i'));
    answer_insights(&mut application, request_id, Ok(worked()));
    row(&screen(&application), "median 48s");
    assert_eq!(
        press(&mut application, 'g'),
        ApplicationTransition::Continue
    );
    assert!(
        screen(&application)
            .iter()
            .any(|row| row.contains("▐Overview▌"))
    );

    let mut keyed = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut keyed);
    answer(&mut keyed, request_id, Ok(report(&query)));
    let (_, _, by_ctrl) = expect_insights_read(ctrl(&mut keyed, KeyCode::Right));
    assert_eq!(by_letter, by_ctrl);
}

#[test]
fn a_new_range_lets_go_of_the_selection_its_figures_were_made_for() {
    let mut application = on_insights(Ok(worked()));
    press_tab(&mut application);
    press_tab(&mut application);
    key(&mut application, KeyCode::Right);
    let focused = turns_border(&application);

    let (_, request_id, _) = expect_insights_read(press(&mut application, '2'));
    answer_insights(&mut application, request_id, Ok(worked()));

    assert!(
        !screen(&application)
            .iter()
            .any(|row| row.contains("<10s · 10 Turns")),
        "no bin is selected"
    );
    assert_eq!(turns_border(&application), focused, "the panel keeps focus");
    key(&mut application, KeyCode::Esc);
    assert_ne!(
        turns_border(&application),
        focused,
        "Esc leaves the panel, with no selection left to clear"
    );
}

#[test]
fn a_refresh_lets_go_of_every_other_range_s_insights() {
    let mut application = on_insights(Ok(worked()));
    let (_, request_id, _) = expect_insights_read(press(&mut application, '2'));
    answer_insights(&mut application, request_id, Ok(worked()));

    let (_, request_id, _) = expect_insights_read(press(&mut application, 'r'));
    answer_insights(&mut application, request_id, Ok(worked()));

    let (_, _, thirty_days) = expect_insights_read(press(&mut application, '3'));
    assert!(thirty_days.interval.contains(afternoon()));
}

#[test]
fn a_tab_whose_read_was_refused_reads_again_when_shown_again() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, _) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Err(UsageMetricsReadRefusal::Failed(
            "the line dropped".to_owned(),
        )),
    );
    let (_, request_id, _) = expect_insights_read(ctrl(&mut application, KeyCode::Right));
    answer_insights(
        &mut application,
        request_id,
        Err(UsageMetricsReadRefusal::Failed(
            "the line dropped".to_owned(),
        )),
    );

    expect_metrics_read(ctrl(&mut application, KeyCode::Left));
    expect_insights_read(ctrl(&mut application, KeyCode::Right));
}

/// The heatmap's row for `day`, from its label to the end of its cells.
fn heat_row(rows: &[String], day: &str) -> String {
    let row = row(rows, &format!("{day}  "));
    let start = row.find(&format!("{day}  ")).expect("the label");
    row[start..].chars().take(5 + 48).collect()
}

/// The cell of `day`'s row at `hour`, a Monday-first weekday name.
fn cell_position(application: &Application, day: &str, hour: u16) -> (u16, u16) {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, &format!("{day}  "));
    (x + 5 + hour * 2, y)
}

#[test]
fn when_you_work_shades_each_local_hour_of_the_week_by_its_share_of_the_busiest() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::WhenYouWork).as_str(),
        "metrics.panel.when-you-work"
    );
    let application = on_insights(Ok(worked()));

    let rows = screen(&application);

    row(&rows, "When you work (Turns by hour)");
    row(&rows, "fewer ░▒▓█ more");
    assert!(
        row(&rows, "00      04      08      12      16      20").contains("00      04"),
        "{rows:#?}"
    );
    assert_eq!(
        heat_row(&rows, "Tue"),
        format!("Tue  {}██▓▓▒▒░░░░{}", "· ".repeat(14), "· ".repeat(5)),
        "the busiest hour shades fullest and each quieter one a level less"
    );
    assert_eq!(
        heat_row(&rows, "Wed"),
        format!("Wed  {}░░", "· ".repeat(23)),
        "a Turn at 23:45 New York time, 03:45 UTC on Thursday, is Wednesday's"
    );
    for day in ["Mon", "Thu", "Fri", "Sat", "Sun"] {
        assert_eq!(heat_row(&rows, day), format!("{day}  {}", "· ".repeat(24)));
    }
    let tue = rows.iter().position(|row| row.contains("Tue  ")).unwrap();
    let mon = rows.iter().position(|row| row.contains("Mon  ")).unwrap();
    let sun = rows.iter().position(|row| row.contains("Sun  ")).unwrap();
    assert_eq!((tue - mon, sun - mon), (1, 6), "weeks begin on Monday");
}

#[test]
fn shading_is_relative_to_the_busiest_hour_however_busy_that_is() {
    let light = on_insights(Ok(worked()));
    let mut heavy_report = worked();
    for quarter in &mut heavy_report.quarter_hours {
        quarter.turns *= 100;
    }
    let heavy = on_insights(Ok(heavy_report));

    for day in ["Tue", "Wed"] {
        assert_eq!(
            heat_row(&screen(&light), day),
            heat_row(&screen(&heavy), day)
        );
    }
}

#[test]
fn a_cell_selected_by_keys_or_by_a_click_states_its_hour_turns_tokens_and_cost() {
    assert_eq!(
        SemanticCommandId::MetricsPanelItemUp.as_str(),
        "metrics.panel.item.up"
    );
    assert_eq!(
        SemanticCommandId::MetricsPanelItemDown.as_str(),
        "metrics.panel.item.down"
    );
    let mut keyed = on_insights(Ok(worked()));
    press_tab(&mut keyed);
    let arrow = |application: &Application, code| {
        application
            .command_for_terminal_input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    };
    for (code, command) in [
        (KeyCode::Left, SemanticCommandId::MetricsPanelItemPrevious),
        (KeyCode::Right, SemanticCommandId::MetricsPanelItemNext),
        (KeyCode::Up, SemanticCommandId::MetricsPanelItemUp),
        (KeyCode::Down, SemanticCommandId::MetricsPanelItemDown),
    ] {
        assert_eq!(
            arrow(&keyed, code),
            Some(CommandId::InvokeSemantic(command))
        );
    }
    // Down to Monday's first hour, then Tuesday's, then along to 14:00.
    key(&mut keyed, KeyCode::Down);
    key(&mut keyed, KeyCode::Down);
    for _ in 0..14 {
        key(&mut keyed, KeyCode::Right);
    }
    let rows = screen(&keyed);
    row(&rows, "Tue 14:00–15:00 · 40 Turns · 2.1M tokens · $14.20");

    // A click selects the cell by its place, Tuesday's fifteenth hour, as
    // the semantic command does, and either of a cell's columns is it.
    let mut invoked = on_insights(Ok(worked()));
    invoke(
        &mut invoked,
        SemanticCommandId::MetricsPanelItemSelect {
            panel: InsightsPanel::WhenYouWork,
            item: 24 + 14,
        },
    );
    assert_eq!(
        screen(&invoked),
        rows,
        "the command selects what the keys did"
    );
    let mut clicked = on_insights(Ok(worked()));
    let cell = cell_position(&clicked, "Tue", 14);
    press_at(&mut clicked, (cell.0 + 1, cell.1));
    assert_eq!(
        screen(&clicked),
        rows,
        "the click selects what the keys did"
    );

    // Up from Wednesday's last hour is Tuesday's, and ← steps back an hour.
    key(&mut clicked, KeyCode::Down);
    for _ in 14..23 {
        key(&mut clicked, KeyCode::Right);
    }
    row(
        &screen(&clicked),
        "Wed 23:00–24:00 · 1 Turn · 500 tokens · $0.05+",
    );
    key(&mut clicked, KeyCode::Up);
    key(&mut clicked, KeyCode::Left);
    row(&screen(&clicked), "Tue 22:00–23:00 · no Turns");
}

#[test]
fn esc_clears_a_selected_cell_before_it_leaves_the_heatmap() {
    let mut application = on_insights(Ok(worked()));
    let cell = cell_position(&application, "Tue", 14);
    press_at(&mut application, cell);
    row(&screen(&application), "Tue 14:00–15:00");
    let focused = heatmap_border(&application);

    key(&mut application, KeyCode::Esc);
    let rows = screen(&application);
    assert!(!rows.iter().any(|row| row.contains("Tue 14:00–15:00")));
    assert_eq!(
        heatmap_border(&application),
        focused,
        "the panel keeps focus"
    );

    key(&mut application, KeyCode::Esc);
    assert_ne!(heatmap_border(&application), focused);
    row(&screen(&application), "When you work");
}

/// The style of the Tools panel's top-left border corner.
fn tools_border(application: &Application) -> Style {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, "Tools used");
    corner_left_of(&buffer, x, y)
}

/// The style of the When you work panel's top-left border corner.
fn heatmap_border(application: &Application) -> Style {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, "When you work");
    corner_left_of(&buffer, x, y)
}

/// The Tools panel's lines beneath its title, the header first, each with
/// its border and runs of spaces closed up.
fn tools_panel(application: &Application) -> Vec<String> {
    let rows = screen(application);
    let title = rows
        .iter()
        .position(|row| row.contains("Tools used"))
        .unwrap_or_else(|| panic!("no Tools panel in\n{}", rows.join("\n")));
    let left = rows[title].find('╭').expect("the panel's corner");
    rows[title + 1..]
        .iter()
        .take_while(|row| !row[left..].trim_start().starts_with('╰'))
        .map(|row| {
            row[left..]
                .split('│')
                .nth(1)
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn focus_tools(application: &mut Application) {
    invoke(
        application,
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Tools),
    );
}

#[test]
fn the_tools_panel_lists_the_most_used_first_and_folds_the_rest_into_more() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Tools).as_str(),
        "metrics.panel.tools"
    );
    let application = on_insights(Ok(worked()));

    assert_eq!(
        tools_panel(&application),
        vec![
            "Tool uses ↓ failed",
            "Command 4,210 212",
            "File Change 2,987 9",
            "Read 2,544 3",
            "github · create_pr 41 2",
            "(4 more)",
        ]
    );
}

#[test]
fn f_sorts_the_tools_by_failures_and_again_by_uses() {
    assert_eq!(
        SemanticCommandId::MetricsToolsSortToggle.as_str(),
        "metrics.tools.sort"
    );
    let mut application = on_insights(Ok(worked()));
    let by_uses = tools_panel(&application);

    assert_eq!(
        press(&mut application, 'f'),
        ApplicationTransition::Continue
    );

    assert_eq!(
        tools_panel(&application),
        vec![
            "Tool uses failed ↓",
            "Command 4,210 212",
            "File Change 2,987 9",
            "Subagent 30 5",
            "fs · write 9 4",
            "(4 more)",
        ]
    );
    press(&mut application, 'f');
    assert_eq!(tools_panel(&application), by_uses);
}

#[test]
fn arrows_scroll_the_tools_into_the_rows_folded_away() {
    let mut application = on_insights(Ok(worked()));
    focus_tools(&mut application);
    for _ in 0..4 {
        key(&mut application, KeyCode::Down);
    }

    // The fourth row selected, the window has yet to move.
    assert_eq!(tools_panel(&application)[5], "(4 more)");
    key(&mut application, KeyCode::Down);
    assert_eq!(
        tools_panel(&application)[1..],
        [
            "File Change 2,987 9",
            "Read 2,544 3",
            "github · create_pr 41 2",
            "Subagent 30 5",
            "(3 more)",
        ]
    );
    for _ in 0..10 {
        key(&mut application, KeyCode::Down);
    }
    assert_eq!(
        tools_panel(&application)[1..],
        [
            "github · create_pr 41 2",
            "Subagent 30 5",
            "Grep 22 0",
            "fs · write 9 4",
            "WebFetch 3 1",
        ],
        "the last row in view, nothing is folded"
    );
    for _ in 0..10 {
        key(&mut application, KeyCode::Up);
    }
    assert_eq!(tools_panel(&application)[1], "Command 4,210 212");
}

#[test]
fn the_wheel_over_the_tools_panel_scrolls_it_without_focusing_it() {
    assert_eq!(
        SemanticCommandId::MetricsPanelScrollDown {
            panel: InsightsPanel::Tools
        }
        .as_str(),
        "metrics.panel.scroll.down"
    );
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let over_tools = text_position(&buffer, "Read");

    assert_eq!(
        wheel(&mut application, MouseEventKind::ScrollDown, over_tools),
        ApplicationTransition::Continue
    );
    assert_eq!(
        tools_panel(&application)[1..],
        [
            "File Change 2,987 9",
            "Read 2,544 3",
            "github · create_pr 41 2",
            "Subagent 30 5",
            "(3 more)",
        ]
    );
    for _ in 0..10 {
        wheel(&mut application, MouseEventKind::ScrollDown, over_tools);
    }
    assert_eq!(tools_panel(&application)[5], "WebFetch 3 1");
    for _ in 0..10 {
        wheel(&mut application, MouseEventKind::ScrollUp, over_tools);
    }
    assert_eq!(
        tools_panel(&application),
        tools_panel(&on_insights(Ok(worked())))
    );
    let rows = screen(&application);
    assert!(
        row(&rows, "30 days").contains("▐30 days▌"),
        "the range stays"
    );
}

#[test]
fn a_click_on_a_tool_selects_it_and_focuses_the_panel() {
    let mut application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let over_tools = text_position(&buffer, "Read");
    wheel(&mut application, MouseEventKind::ScrollDown, over_tools);
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let subagent = text_position(&buffer, "Subagent");

    press_at(&mut application, subagent);
    // Down from the clicked row, the fifth, scrolls the sixth into view.
    key(&mut application, KeyCode::Down);

    assert_eq!(
        tools_panel(&application)[1..],
        [
            "Read 2,544 3",
            "github · create_pr 41 2",
            "Subagent 30 5",
            "Grep 22 0",
            "(2 more)",
        ]
    );
}

#[test]
fn a_range_whose_turns_used_no_tool_says_so_in_the_panel() {
    let application = on_insights(Ok(InsightsReport::default()));

    assert_eq!(
        tools_panel(&application)[0],
        "No Tools used in the last 30 days"
    );
}
