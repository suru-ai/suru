//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Outlook's Server worked over the range the Overview tab shares with it.

use super::{
    HEIGHT, NEW_YORK, WIDTH, afternoon, answer, ask, press, press_at, reader_in, report, row,
    screen, wheel, zone,
};
use crate::tui_rendering::support::{
    application_looking_at_studio, grace_elapses, invoke, key, rendered_application_buffer,
    studio_stops_answering, text_position,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::{buffer::Buffer, style::Style};
use suru::{
    protocol::{
        InsightsQuery, InsightsReport, Outlook, TurnDurations, TurnInsights, UsageMetricsInterval,
        UsageMetricsQuery,
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
        quarter_hours: Vec::new(),
    }
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
    let (x, y) = text_position(&buffer, "Turns");
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
    let focused = turns_border(&application);
    assert_ne!(focused, unfocused, "the focused panel's border stands out");
    assert_eq!(
        key(&mut application, KeyCode::BackTab),
        ApplicationTransition::Continue
    );
    assert_eq!(
        turns_border(&application),
        focused,
        "with one panel, focus moves round to it"
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
