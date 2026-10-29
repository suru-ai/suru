//! The Insights tab of the Usage Metrics view: how the Sessions on the
//! Outlook's Server worked over the range the Overview tab shares with it.

use super::{
    HEIGHT, NEW_YORK, WIDTH, afternoon, answer, ask, expect_read, in_new_york, press, press_at,
    reader_in, remove_studio, report, row, screen, wheel, zone,
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
        ApprovalInsights, CacheInsights, CompactionCounts, CompactionInsights, Cost, CountedCost,
        DelegationInsights, EstimatedSaving, FreedShare, InsightsQuery, InsightsReport,
        InsightsTool, Outlook, QUARTER_HOUR_MS, QuarterHourTurns, ToolTally, TurnDurations,
        TurnInsights, UsageMetricsInterval, UsageMetricsQuery,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, InsightsPanel,
        MetricsReadRefusal, MetricsTab, SemanticCommandId,
    },
};
use uuid::Uuid;

mod stacked;
mod workspaces;

fn ctrl(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
        .expect("deliver a key press")
}

fn command_for_ctrl(application: &Application, code: KeyCode) -> Option<CommandId> {
    application
        .command_for_terminal_input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
}

/// The two reads showing Insights asks for together: its own, and the
/// one-bucket Usage Metrics read its Workspaces panel fills from.
struct InsightsReads {
    origin: Outlook,
    request_id: Uuid,
    insights: InsightsQuery,
    workspaces_request_id: Uuid,
    workspaces: UsageMetricsQuery,
}

fn expect_insights_reads(transition: ApplicationTransition) -> InsightsReads {
    match transition {
        ApplicationTransition::ReadInsightsAndWorkspaces {
            origin,
            request_id,
            query,
            workspaces_request_id,
            workspaces,
        } => InsightsReads {
            origin,
            request_id,
            insights: query,
            workspaces_request_id,
            workspaces,
        },
        other => panic!("the tab reads the Outlook's Server's Insights, not {other:?}"),
    }
}

fn expect_insights_read(transition: ApplicationTransition) -> (Outlook, Uuid, InsightsQuery) {
    let reads = expect_insights_reads(transition);
    (reads.origin, reads.request_id, reads.insights)
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
    result: Result<InsightsReport, MetricsReadRefusal>,
) {
    application
        .handle_event(ApplicationEvent::InsightsRead { request_id, result })
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
        delegation: worked_delegation(),
        cache: worked_cache(),
    }
}

/// The mockup's cache: 71% of 10M tokens of input read from cache, which
/// saved about $1,104 at today's rates.
fn worked_cache() -> CacheInsights {
    CacheInsights {
        fresh_input_tokens: Some(1_400_000),
        cache_read_tokens: Some(7_100_000),
        cache_write_tokens: Some(1_500_000),
        saving: EstimatedSaving {
            nano_usd: Some(1_104_000_000_000),
            is_partial: false,
        },
    }
}

/// What `usd` dollars cost a role, partial where `partial` says.
fn role(usd: f64, partial: bool) -> CountedCost {
    CountedCost {
        amount: Cost::from_usd(usd),
        is_partial: partial,
    }
}

/// The mockup's $413.27 divided: 71% the user's own Sessions', 22% their
/// Subagents', 9% of it brokered, 6% Sidekicks', 1% Subsessions', and under
/// 1% Suru's own Errands'.
fn worked_delegation() -> DelegationInsights {
    DelegationInsights {
        own_sessions: role(293.13, false),
        native_subagents: role(53.67, false),
        brokered_subagents: role(37.16, false),
        sidekicks: role(24.77, false),
        subsessions: role(4.13, false),
        errands: role(0.41, false),
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
        cost: CountedCost {
            amount: Cost::from_usd(usd),
            is_partial: false,
        },
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
    let mut partial = new_york_quarter(21, 23, 45, 1, 500, 0.05);
    partial.cost.is_partial = true;
    hours.push(partial);
    hours
}

/// The view opened on 30 days, its Overview answered, then Insights shown,
/// asked, and answered with `insights`.
fn on_insights(insights: Result<InsightsReport, MetricsReadRefusal>) -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let reads = expect_insights_reads(ctrl(&mut application, KeyCode::Right));
    answer_insights(&mut application, reads.request_id, insights);
    let workspaces = workspaces::spent(&reads.workspaces, &workspaces::MOCKUP);
    answer(
        &mut application,
        reads.workspaces_request_id,
        Ok(workspaces),
    );
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
    border(application, " Turns ")
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
        "metrics.panel.focus.turns"
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
    let heatmap_unfocused = heatmap_border(&application);

    assert_eq!(press_tab(&mut application), ApplicationTransition::Continue);
    assert_ne!(
        heatmap_border(&application),
        heatmap_unfocused,
        "focus begins at the first panel"
    );
    assert_eq!(turns_border(&application), unfocused);
    press_tab(&mut application);
    let focused = turns_border(&application);
    assert_ne!(focused, unfocused, "the focused panel's border stands out");
    assert_eq!(
        key(&mut application, KeyCode::BackTab),
        ApplicationTransition::Continue
    );
    assert_eq!(
        (heatmap_border(&application), turns_border(&application)),
        (focused, unfocused)
    );
    key(&mut application, KeyCode::BackTab);
    assert_eq!(
        (
            border(&application, " Delegation "),
            heatmap_border(&application)
        ),
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

/// Tabs through When you work to the Turns panel.
fn tab_to_turns(application: &mut Application) {
    for _ in 0..2 {
        press_tab(application);
    }
}

/// The style of the top-left border corner of the panel titled `title`.
fn border(application: &Application, title: &str) -> Style {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, title);
    corner_left_of(&buffer, x, y)
}

#[test]
fn esc_clears_the_selection_then_leaves_the_panel_then_closes_the_view() {
    let mut application = on_insights(Ok(worked()));
    let unfocused = turns_border(&application);
    tab_to_turns(&mut application);
    let focused = turns_border(&application);
    key(&mut application, KeyCode::Right);
    key(&mut application, KeyCode::Right);
    row(&screen(&application), "10–30s · 30 · ");

    key(&mut application, KeyCode::Esc);
    let rows = screen(&application);
    assert!(!rows.iter().any(|row| row.contains("10–30s · 30 · ")));
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

    // The Turns column is too narrow at 120 columns for the word "Turns".
    row(&screen(&application), "1–2m · 80 · 26%");
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
    // As briefly as the mockup at 120 columns, where the Turns column is
    // narrow; the longest and the endings' names stand where it is wider.
    row(&rows, "median 48s · p90 6m 12s  ");
    row(&rows, "▁▃▇█▆▃▂▁  <10s … 30m+");
    row(&rows, "✓ 1,213  ✗ 40  ⊘ 31  ");
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

    row(&rows, "No durations recorded");
    row(&rows, "✓ 3  ✗ 0  ⊘ 0  ");
}

#[test]
fn a_refused_read_shows_its_reason_in_place_of_the_panels() {
    let application = on_insights(Err(MetricsReadRefusal::Refused(
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

/// Answers both reads showing the tab asked for: `insights`, and the
/// mockup's Workspaces beside them.
fn answer_reads(application: &mut Application, reads: &InsightsReads, insights: InsightsReport) {
    answer_insights(application, reads.request_id, Ok(insights));
    let workspaces = workspaces::spent(&reads.workspaces, &workspaces::MOCKUP);
    answer(application, reads.workspaces_request_id, Ok(workspaces));
}

#[test]
fn turning_the_outlook_beneath_insights_reads_both_tabs_from_the_server_it_now_names() {
    let mut application = application_looking_at_studio()
        .with_session_clock(afternoon)
        .with_time_zone(zone(NEW_YORK));
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let reads = expect_insights_reads(ctrl(&mut application, KeyCode::Right));
    answer_reads(&mut application, &reads, worked());

    let reads = expect_insights_reads(remove_studio(&mut application));

    assert_eq!(reads.origin, Outlook::Local);
    let rows = screen(&application);
    row(&rows, "Reading Insights…");
    assert!(
        !rows.iter().any(|row| row.contains("median 48s")),
        "{rows:#?}"
    );
    answer_reads(&mut application, &reads, worked());
    let (origin, _, _) = expect_read(ctrl(&mut application, KeyCode::Left));
    assert_eq!(
        origin,
        Outlook::Local,
        "studio's Overview went with the Outlook, so this machine's is read"
    );
}

#[test]
fn a_refresh_lets_go_of_the_other_tab_s_figures_so_the_tabs_never_disagree() {
    let mut application = on_insights(Ok(worked()));

    let reads = expect_insights_reads(press(&mut application, 'r'));
    answer_reads(&mut application, &reads, worked());
    let (_, request_id, query) = expect_read(ctrl(&mut application, KeyCode::Left));
    answer(&mut application, request_id, Ok(report(&query)));

    let (_, request_id, query) = expect_read(press(&mut application, 'r'));
    answer(&mut application, request_id, Ok(report(&query)));
    expect_insights_reads(ctrl(&mut application, KeyCode::Right));
}

/// What Insights says of All time where Turns stored before Suru timed
/// them lie outside it.
const UNTIMED_NOTE: &str = "Turns recorded before Suru timed Turns are not included";

/// [`on_insights`] of All time, its Workspace read saying `untimed_turns`
/// Turns are untimed, and the 30 days before it read alike.
fn all_time_with_untimed(untimed_turns: u64) -> (Application, Vec<String>) {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let mut thirty_days = Vec::new();
    for key in [None, Some('5')] {
        let reads = expect_insights_reads(match key {
            None => ctrl(&mut application, KeyCode::Right),
            Some(key) => press(&mut application, key),
        });
        answer_insights(&mut application, reads.request_id, Ok(worked()));
        let mut workspaces = workspaces::spent(&reads.workspaces, &workspaces::MOCKUP);
        workspaces.untimed.total.turns = untimed_turns;
        answer(
            &mut application,
            reads.workspaces_request_id,
            Ok(workspaces),
        );
        if key.is_none() {
            thirty_days = screen(&application);
        }
    }
    (application, thirty_days)
}

#[test]
fn all_time_says_turns_from_before_suru_timed_them_are_not_included() {
    let (application, thirty_days) = all_time_with_untimed(3);

    row(&screen(&application), UNTIMED_NOTE);
    assert!(
        !thirty_days.join("\n").contains(UNTIMED_NOTE),
        "only All time reaches back that far: {thirty_days:#?}"
    );

    let (application, _) = all_time_with_untimed(0);
    let rows = screen(&application);
    assert!(!rows.join("\n").contains(UNTIMED_NOTE), "{rows:#?}");
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
    tab_to_turns(&mut application);
    key(&mut application, KeyCode::Right);
    let focused = turns_border(&application);

    let (_, request_id, _) = expect_insights_read(press(&mut application, '2'));
    answer_insights(&mut application, request_id, Ok(worked()));

    assert!(
        !screen(&application)
            .iter()
            .any(|row| row.contains("<10s · 10 · ")),
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
        Err(MetricsReadRefusal::Failed("the line dropped".to_owned())),
    );
    let (_, request_id, _) = expect_insights_read(ctrl(&mut application, KeyCode::Right));
    answer_insights(
        &mut application,
        request_id,
        Err(MetricsReadRefusal::Failed("the line dropped".to_owned())),
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
        "metrics.panel.focus.when-you-work"
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

/// The style of the When you work panel's top-left border corner.
fn heatmap_border(application: &Application) -> Style {
    border(application, "When you work")
}

/// The lines of the panel titled `title` beneath its title, inside its
/// border, each with runs of spaces closed up.
fn panel_lines(application: &Application, title: &str) -> Vec<String> {
    panel_lines_at(application, title, WIDTH)
}

/// The lines of the panel titled `title`, as [`panel_lines`] has them, in a
/// frame `width` columns wide.
fn panel_lines_at(application: &Application, title: &str, width: u16) -> Vec<String> {
    let buffer = rendered_application_buffer(application, width, HEIGHT);
    let (x, y) = text_position(&buffer, title);
    let symbol = |column: u16, row: u16| buffer[(column, row)].symbol().to_owned();
    let left = (0..x)
        .rev()
        .find(|column| symbol(*column, y) == "╭")
        .unwrap_or_else(|| panic!("no corner left of {title:?}"));
    let right = (x..buffer.area.width)
        .find(|column| symbol(*column, y) == "╮")
        .unwrap_or_else(|| panic!("no corner right of {title:?}"));
    (y + 1..buffer.area.height)
        .take_while(|row| symbol(left, *row) != "╰")
        .map(|row| {
            (left + 1..right)
                .map(|column| symbol(column, row))
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// The Tools panel's lines beneath its title, the header first.
fn tools_panel(application: &Application) -> Vec<String> {
    panel_lines(application, "Tools used")
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
        "metrics.panel.focus.tools"
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
        "metrics.tools.sort.toggle"
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

/// Every panel's title, in the order the mockup reads them: down its left
/// column beneath When you work, then down its middle one, then down its
/// right-hand column.
const READING_ORDER: [&str; 8] = [
    "When you work",
    " Turns ",
    " Cache ",
    " Compactions ",
    " Approvals ",
    "Workspaces by Cost",
    "Tools used",
    " Delegation ",
];

/// Where the top-left border corner of the panel titled `title` stands.
fn corner_at(application: &Application, title: &str) -> (u16, u16) {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, title);
    let corner = (0..x)
        .rev()
        .find(|column| buffer[(*column, y)].symbol() == "╭")
        .expect("the panel's corner");
    (corner, y)
}

#[test]
fn panels_stand_as_the_mockup_lays_them_out() {
    let application = on_insights(Ok(worked()));
    let [
        heatmap,
        turns,
        cache,
        compactions,
        approvals,
        workspaces,
        tools,
        delegation,
    ] = READING_ORDER.map(|title| corner_at(&application, title));

    assert_eq!(
        workspaces.1, heatmap.1,
        "Workspaces stand beside When you work"
    );
    assert!(workspaces.0 > compactions.0 && compactions.0 > turns.0);
    assert_eq!(
        (turns.0, cache.0),
        (heatmap.0, heatmap.0),
        "Turns, then Cache, stand beneath When you work, at its left edge"
    );
    assert!(heatmap.1 < turns.1 && turns.1 < cache.1);
    assert_eq!(
        (compactions.1, approvals.0),
        (turns.1, compactions.0),
        "Compactions stand beside Turns beneath When you work, Approvals beneath them"
    );
    assert!(approvals.1 > compactions.1);
    assert_eq!(
        (tools.0, delegation.0),
        (workspaces.0, workspaces.0),
        "Tools, then Delegation, stand beneath Workspaces in the right-hand column"
    );
    assert!(workspaces.1 < tools.1 && tools.1 < delegation.1);
    assert!(
        tools.1 < turns.1 && approvals.1 < cache.1,
        "each column stacks its own panels, none waiting on its neighbour's height"
    );
}

#[test]
fn tab_moves_focus_through_the_panels_in_the_order_they_are_read() {
    let mut application = on_insights(Ok(worked()));
    let unfocused = READING_ORDER.map(|title| border(&application, title));

    for (place, title) in READING_ORDER.iter().enumerate() {
        press_tab(&mut application);
        let focused: Vec<&str> = READING_ORDER
            .iter()
            .zip(&unfocused)
            .filter(|(title, unfocused)| border(&application, title) != **unfocused)
            .map(|(title, _)| *title)
            .collect();
        assert_eq!(focused, vec![*title], "Tab {}", place + 1);
    }
    press_tab(&mut application);
    assert_ne!(
        border(&application, READING_ORDER[0]),
        unfocused[0],
        "from the last panel, focus moves round to the first"
    );
}

#[test]
fn the_compactions_panel_states_how_many_how_many_were_manual_and_failed_and_what_they_freed() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Compactions).as_str(),
        "metrics.panel.focus.compactions"
    );
    let application = on_insights(Ok(worked()));

    assert_eq!(
        panel_lines(&application, " Compactions "),
        vec!["27 · 6 manual · ✗ 1 failed", "frees 61% on average"]
    );
}

#[test]
fn compactions_that_could_not_say_what_they_freed_leave_it_unstated() {
    let mut report = worked();
    report.compactions = CompactionInsights {
        manual: CompactionCounts {
            interrupted: 1,
            ..CompactionCounts::default()
        },
        ..CompactionInsights::default()
    };
    let application = on_insights(Ok(report));

    assert_eq!(
        panel_lines(&application, " Compactions "),
        vec!["1 · 1 manual · ✗ 0 failed", "No record of what it freed"]
    );
}

#[test]
fn compactions_that_left_more_context_than_they_found_say_so() {
    let mut report = worked();
    report.compactions.freed = Some(FreedShare {
        compactions: 2,
        average_millionths: -50_000,
    });
    let application = on_insights(Ok(report));

    assert_eq!(
        panel_lines(&application, " Compactions ")[1],
        "leaves 5% more on average"
    );
}

#[test]
fn compactions_that_left_less_than_half_a_percent_more_free_none() {
    let mut report = worked();
    report.compactions.freed = Some(FreedShare {
        compactions: 2,
        average_millionths: -4_999,
    });
    let application = on_insights(Ok(report));

    assert_eq!(
        panel_lines(&application, " Compactions ")[1],
        "frees 0% on average"
    );
}

#[test]
fn a_range_with_no_compactions_says_so_in_the_panel() {
    let application = on_insights(Ok(InsightsReport::default()));

    assert_eq!(
        panel_lines(&application, " Compactions ")[0],
        "No Compactions"
    );
}

#[test]
fn the_approvals_panel_states_how_many_were_asked_and_how_each_was_decided() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Approvals).as_str(),
        "metrics.panel.focus.approvals"
    );
    let application = on_insights(Ok(worked()));

    assert_eq!(
        panel_lines(&application, " Approvals "),
        vec![
            "188 asked",
            "✓ 171 ✗ 12 ⧗ 5",
            "11 for the Session",
            "2 interrupted the Turn",
        ]
    );
}

#[test]
fn a_range_with_no_approvals_says_so_in_the_panel() {
    let application = on_insights(Ok(InsightsReport::default()));

    assert_eq!(
        panel_lines(&application, " Approvals ")[0],
        "No Approvals asked"
    );
}

fn delegation_panel(application: &Application) -> Vec<String> {
    panel_lines(application, "Delegation (Cost)")
}

#[test]
fn the_delegation_panel_divides_cost_by_role_with_brokered_subagents_beside_theirs() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Delegation).as_str(),
        "metrics.panel.focus.delegation"
    );
    let application = on_insights(Ok(worked()));

    assert_eq!(
        delegation_panel(&application),
        vec![
            "own Sessions 71% $293.13 ████████████",
            "Subagents 22% $90.83 ███▊ brokered 9%",
            "Sidekicks 6% $24.77 █",
            "Subsessions 1% $4.13 ▏",
            "Errands <1% $0.41 ▏",
            "A Claude Session's Subagents are in its Cost",
        ]
    );
}

#[test]
fn the_delegation_panel_s_errands_row_states_suru_s_own_cost_in_the_errands_colour() {
    let mut report = worked();
    report.delegation.errands = role(41.33, true);
    let application = on_insights(Ok(report));

    let lines = panel_lines_at(&application, "Delegation (Cost)", 140);
    assert_eq!(lines[4], "Errands 9%+ $41.33+ ██▌");
    let buffer = rendered_application_buffer(&application, 140, HEIGHT);
    let bar = |label: &str| {
        let (x, y) = text_position(&buffer, label);
        let x = (x..buffer.area.right())
            .find(|x| buffer[(*x, y)].symbol() == "█")
            .expect("the row has a bar");
        buffer[(x, y)].fg
    };
    let errands = bar("Errands");
    assert_ne!(
        errands,
        bar("own Sessions"),
        "the Errands bar takes the colour reserved for them, as on Overview"
    );
    assert_eq!(bar("Sidekicks"), bar("own Sessions"));
}

#[test]
fn a_role_whose_cost_is_partial_says_so_with_a_plus() {
    let mut report = worked();
    report.delegation.brokered_subagents.is_partial = true;
    report.delegation.subsessions = CountedCost {
        amount: None,
        is_partial: true,
    };
    report.delegation.sidekicks = CountedCost::default();
    let application = on_insights(Ok(report));

    // Wide enough for every Cost beside the bars and the brokered share.
    let lines = panel_lines_at(&application, "Delegation (Cost)", 140);
    assert_eq!(
        lines[1], "Subagents 24%+ $90.83+ ██████▍ brokered 10%+",
        "partial where its brokered part is, and its shares of a partial whole"
    );
    assert_eq!(lines[2], "Sidekicks 0% —", "no work, so no Cost to state");
    assert_eq!(
        lines[3], "Subsessions 0%+ unpriced",
        "work, none of it priced, is never a zero"
    );
}

#[test]
fn a_range_with_no_cost_says_so_in_the_delegation_panel() {
    let application = on_insights(Ok(InsightsReport::default()));
    assert_eq!(
        delegation_panel(&application)[0],
        "No Cost in the last 30 days"
    );

    let mut report = InsightsReport::default();
    report.delegation.own_sessions.is_partial = true;
    let application = on_insights(Ok(report));
    assert_eq!(
        delegation_panel(&application)[0],
        "No work priced in the last 30 days"
    );
}

fn cache_panel(application: &Application) -> Vec<String> {
    panel_lines(application, " Cache ")
}

#[test]
fn the_cache_panel_states_the_share_read_from_cache_and_the_estimated_saving() {
    assert_eq!(
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Cache).as_str(),
        "metrics.panel.focus.cache"
    );
    let application = on_insights(Ok(worked()));

    assert_eq!(
        cache_panel(&application),
        vec!["71% of input from cache", "saved ≈ $1,104.00 (est.)"]
    );
}

#[test]
fn a_saving_some_model_had_no_rates_for_says_so_with_a_plus() {
    let mut report = worked();
    report.cache.saving.is_partial = true;
    let application = on_insights(Ok(report));

    assert_eq!(cache_panel(&application)[1], "saved ≈ $1,104.00+ (est.)");
}

#[test]
fn cache_writes_that_cost_more_than_reads_saved_save_less_than_nothing() {
    let mut report = worked();
    report.cache.saving = EstimatedSaving {
        nano_usd: Some(-2_000_000_000),
        is_partial: false,
    };
    let application = on_insights(Ok(report));

    assert_eq!(cache_panel(&application)[1], "saved ≈ −$2.00 (est.)");
}

#[test]
fn a_saving_no_model_s_rates_could_estimate_is_left_unstated() {
    let mut report = worked();
    report.cache.saving = EstimatedSaving {
        nano_usd: None,
        is_partial: true,
    };
    let application = on_insights(Ok(report));

    assert_eq!(cache_panel(&application)[1], "No rates for an estimate");
}

#[test]
fn a_range_with_no_input_says_so_in_the_cache_panel() {
    let application = on_insights(Ok(InsightsReport::default()));

    assert_eq!(cache_panel(&application)[0], "No input");
}

#[test]
fn delegation_leaves_its_costs_out_where_they_would_crowd_the_brokered_share() {
    let mut report = worked();
    report.delegation.brokered_subagents.is_partial = true;
    let application = on_insights(Ok(report));

    assert_eq!(
        delegation_panel(&application)[..2],
        [
            "own Sessions 71%+ ████████████████████▋",
            "Subagents 22%+ ██████▍ brokered 9%+"
        ]
    );
}

/// The widest frame the small panels stack in beneath When you work, in a
/// column as wide as its hours need.
const STACKED_WIDTH: u16 = 113;

#[test]
fn below_the_mockup_s_width_the_small_panels_stack_and_say_it_all_where_they_fit() {
    let application = on_insights(Ok(worked()));
    let buffer = rendered_application_buffer(&application, STACKED_WIDTH, HEIGHT);
    let corner = |title: &str| {
        let (x, y) = text_position(&buffer, title);
        let corner = (0..x)
            .rev()
            .find(|column| buffer[(*column, y)].symbol() == "╭")
            .expect("the panel's corner");
        (corner, y)
    };
    let left: Vec<(u16, u16)> = READING_ORDER[..5]
        .iter()
        .map(|title| corner(title))
        .collect();

    assert!(
        left.iter().all(|(x, _)| *x == left[0].0) && left.windows(2).all(|two| two[0].1 < two[1].1),
        "When you work, Turns, Cache, Compactions, and Approvals stack in one column: {left:?}"
    );
    let lines = |title| panel_lines_at(&application, title, STACKED_WIDTH);
    assert_eq!(
        lines(" Turns ")
            .into_iter()
            .map(|line| line.replace("▁▃▇█▆▃▂▁ ", ""))
            .collect::<Vec<_>>(),
        [
            "median 48s · p90 6m 12s · longest 41m 5s",
            "<10s … 30m+",
            "✓ 1,213 completed ✗ 40 failed ⊘ 31 interrupted",
        ]
    );
    assert_eq!(
        lines(" Compactions "),
        [
            "27 Compactions · 6 manual · ✗ 1 failed",
            "frees 61% of context on average",
        ]
    );
    assert_eq!(
        lines(" Approvals "),
        [
            "188 asked",
            "✓ 171 accepted ✗ 12 declined ⧗ 5 waiting or expired",
            "11 for the Session · 2 also interrupted the Turn",
        ],
        "only the rows it fills"
    );
    assert_eq!(
        lines(" Cache "),
        ["71% of input from cache", "saved ≈ $1,104.00 (est.)"]
    );
}

#[test]
fn a_wide_cache_panel_says_in_full_what_it_has_no_figure_for() {
    let cache_lines =
        |report: InsightsReport| panel_lines_at(&on_insights(Ok(report)), " Cache ", STACKED_WIDTH);
    let mut unrated = worked();
    unrated.cache.saving = EstimatedSaving {
        nano_usd: None,
        is_partial: true,
    };
    let mut uncached = worked();
    uncached.cache.saving = EstimatedSaving::default();

    assert_eq!(cache_lines(unrated)[1], "No rates to estimate a saving at");
    assert_eq!(
        cache_lines(uncached)[1],
        "Nothing read from or written to cache"
    );
    assert_eq!(
        cache_lines(InsightsReport::default())[0],
        "No input in the last 30 days"
    );
}
