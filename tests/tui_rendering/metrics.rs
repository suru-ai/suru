//! `/metrics`: the Usage Metrics of every Session on the Server the Outlook is
//! turned toward, drawn over the whole frame.

use crate::tui_rendering::support::{
    application_looking_at_studio, click_mouse, connected_application, deliver_settings,
    enter_session, grace_elapses, invoke, key, rendered_application_buffer,
    rendered_application_rows_at, studio_stops_answering, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use jiff::{ToSpan, civil::date, tz::TimeZone};
use ratatui::style::Color;
use suru::{
    managed_client::{ManagedEvent, RecoveryStatus},
    protocol::{
        Cost, EffectiveSettings, KeyedUsageMetrics, ModelId, Outlook, ProviderId, SessionTimestamp,
        SidebarVisibility, UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsBucket,
        UsageMetricsInterval, UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery,
        UsageMetricsReport,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, MetricsMeasure,
        MetricsRange, SemanticCommandId, UsageMetricsReadRefusal,
    },
};
use uuid::Uuid;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
const NEW_YORK: &str = "America/New_York";

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap_or_else(|error| panic!("the zone {name}: {error}"))
}

/// The moment a wall-clock reading in New York names.
fn in_new_york(wall: jiff::civil::DateTime) -> SessionTimestamp {
    let zoned = wall.to_zoned(zone(NEW_YORK)).expect("a wall-clock reading");
    SessionTimestamp(zoned.timestamp().as_millisecond() as u64)
}

/// An Application whose reader is in `zone_name` at `now`.
fn reader_in(zone_name: &str, now: SessionTimestamp) -> Application {
    let workspace = workspace_dir();
    connected_application(workspace.path())
        .with_session_clock(move || now)
        .with_time_zone(zone(zone_name))
}

/// Mid-afternoon on 28 October 2026 in New York.
fn afternoon() -> SessionTimestamp {
    in_new_york(date(2026, 10, 28).at(14, 30, 0, 0))
}

/// Types `/metrics` and presses Enter, answering what the press asked of
/// which Server.
fn ask(application: &mut Application) -> (Outlook, Uuid, UsageMetricsQuery) {
    type_terminal_text(application, "/metrics");
    expect_read(key(application, KeyCode::Enter))
}

fn expect_read(transition: ApplicationTransition) -> (Outlook, Uuid, UsageMetricsQuery) {
    match transition {
        ApplicationTransition::ReadUsageMetrics {
            origin,
            request_id,
            query,
        } => (origin, request_id, query),
        other => panic!("the view reads the Outlook's Server, not {other:?}"),
    }
}

fn answer(
    application: &mut Application,
    request_id: Uuid,
    result: Result<UsageMetricsReport, UsageMetricsReadRefusal>,
) {
    application
        .handle_event(ApplicationEvent::UsageMetricsRead { request_id, result })
        .expect("deliver the Usage Metrics");
}

fn model(provider: &str, model: &str) -> UsageMetricsKey {
    UsageMetricsKey::Model {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
    }
}

fn opus() -> UsageMetricsKey {
    model("claude", "claude-opus-5-5")
}

fn codex() -> UsageMetricsKey {
    model("codex", "gpt-5.5-codex")
}

fn copilot() -> UsageMetricsKey {
    model("copilot", "gpt-5.5")
}

fn tokens(tokens: u64) -> UsageMetricsAggregate {
    UsageMetricsAggregate {
        fresh_input_tokens: Some(tokens),
        ..UsageMetricsAggregate::default()
    }
}

/// Thirty days of three Models: Opus takes 1M to 5M a day in turn, Codex 0
/// to 1M, and Copilot 250K every other day. Over the range that is 90M,
/// 15M, and 3.75M, and the busiest day, the fifteenth, holds 6.25M.
///
/// Two Sessions work on Opus every day, one of them on Codex too on the
/// days Codex works, and a third on Copilot on its days, so the busiest day
/// stacks four Sessions over three. Over the range twelve Sessions worked
/// on Opus, four of them on Codex too, and three more on Copilot: the
/// Models count nineteen Sessions, which are fifteen.
fn report(query: &UsageMetricsQuery) -> UsageMetricsReport {
    let buckets = (0..query.buckets.len() as u64)
        .map(|day| {
            let parts = [
                (opus(), (day % 5 + 1) * 1_000_000, 2),
                (codex(), (day % 3) * 500_000, u64::from(day % 3 != 0)),
                (
                    copilot(),
                    if day % 2 == 0 { 250_000 } else { 0 },
                    u64::from(day % 2 == 0),
                ),
            ];
            UsageMetricsBucket {
                total: UsageMetricsAggregate {
                    sessions: 2 + u64::from(day % 2 == 0),
                    ..tokens(parts.iter().map(|(_, tokens, _)| tokens).sum())
                },
                by_key: parts
                    .into_iter()
                    .map(|(key, tokens_, sessions)| KeyedUsageMetrics {
                        key,
                        aggregate: UsageMetricsAggregate {
                            sessions,
                            ..tokens(tokens_)
                        },
                    })
                    .collect(),
            }
        })
        .collect();
    let mut report = UsageMetricsReport {
        buckets,
        untimed: UsageMetricsBucket::default(),
        across_buckets_and_untimed: UsageMetricsAggregate::default(),
        comparison: None,
        earliest_turn_at: query.buckets.first().map(|bucket| bucket.start),
        // Listed smallest first, so the chart's order is its own.
        keys: vec![
            UsageMetricsKeyLabel {
                key: copilot(),
                label: "copilot · GPT-5.5".to_owned(),
            },
            UsageMetricsKeyLabel {
                key: codex(),
                label: "GPT-5.5 Codex".to_owned(),
            },
            UsageMetricsKeyLabel {
                key: opus(),
                label: "Claude Opus 5.5".to_owned(),
            },
        ],
        across_buckets: UsageMetricsBucket {
            total: UsageMetricsAggregate {
                sessions: 15,
                ..UsageMetricsAggregate::default()
            },
            by_key: [(opus(), 12), (codex(), 4), (copilot(), 3)]
                .into_iter()
                .map(|(key, sessions)| KeyedUsageMetrics {
                    key,
                    aggregate: UsageMetricsAggregate {
                        sessions,
                        ..UsageMetricsAggregate::default()
                    },
                })
                .collect(),
        },
    };
    take_together(&mut report);
    report
}

/// Sets `report`'s figures across its buckets, and across them and its
/// untimed Turns, which it holds none of, to the buckets taken together, as
/// the Server does, in the figures that add up across buckets: Usage and
/// Cost. Session counts do not, so it keeps those it was given.
fn take_together(report: &mut UsageMetricsReport) {
    fn add(into: &mut UsageMetricsAggregate, part: &UsageMetricsAggregate) {
        let count = |held: Option<u64>, more: Option<u64>| match (held, more) {
            (Some(held), Some(more)) => Some(held + more),
            (held, more) => held.or(more),
        };
        let cost = |held: Option<Cost>, more: Option<Cost>| match (held, more) {
            (Some(held), Some(more)) => Some(held.saturating_add(more)),
            (held, more) => held.or(more),
        };
        into.fresh_input_tokens = count(into.fresh_input_tokens, part.fresh_input_tokens);
        into.cache_read_tokens = count(into.cache_read_tokens, part.cache_read_tokens);
        into.cache_write_tokens = count(into.cache_write_tokens, part.cache_write_tokens);
        into.output_tokens = count(into.output_tokens, part.output_tokens);
        into.reasoning_tokens = count(into.reasoning_tokens, part.reasoning_tokens);
        into.cost = cost(into.cost, part.cost);
        into.estimated_cost = cost(into.estimated_cost, part.estimated_cost);
        into.is_cost_partial |= part.is_cost_partial;
    }
    let counted = |aggregate: &UsageMetricsAggregate| UsageMetricsAggregate {
        sessions: aggregate.sessions,
        subagent_sessions: aggregate.subagent_sessions,
        ..UsageMetricsAggregate::default()
    };
    let mut total = counted(&report.across_buckets.total);
    let mut by_key: Vec<KeyedUsageMetrics> = report
        .keys
        .iter()
        .map(|label| KeyedUsageMetrics {
            key: label.key.clone(),
            aggregate: report
                .across_buckets
                .by_key
                .iter()
                .find(|keyed| keyed.key == label.key)
                .map(|keyed| counted(&keyed.aggregate))
                .unwrap_or_default(),
        })
        .collect();
    for bucket in &report.buckets {
        add(&mut total, &bucket.total);
        for part in &bucket.by_key {
            if let Some(keyed) = by_key.iter_mut().find(|keyed| keyed.key == part.key) {
                add(&mut keyed.aggregate, &part.aggregate);
            }
        }
    }
    report.across_buckets_and_untimed = total;
    report.across_buckets = UsageMetricsBucket { total, by_key };
}

/// Shows the Tokens measure, which reads nothing again.
fn show_tokens(application: &mut Application) {
    assert_eq!(press(application, 't'), ApplicationTransition::Continue);
}

fn screen(application: &Application) -> Vec<String> {
    rendered_application_rows_at(application, WIDTH, HEIGHT)
}

fn row<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row holds {needle:?} in\n{}", rows.join("\n")))
}

fn press_at(application: &mut Application, (column, row): (u16, u16)) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the pointer")
}

#[test]
fn slash_metrics_reads_the_last_thirty_local_days_by_model() {
    assert_eq!(SemanticCommandId::MetricsShow.as_str(), "metrics.show");
    let mut application = reader_in(NEW_YORK, afternoon());

    let (origin, _, query) = ask(&mut application);

    assert_eq!(origin, Outlook::Local);
    assert_eq!(query.breakdown, UsageMetricsBreakdown::Model);
    let midnights: Vec<SessionTimestamp> = (0..=30)
        .map(|day| {
            in_new_york(
                date(2026, 9, 29)
                    .checked_add(day.days())
                    .unwrap()
                    .at(0, 0, 0, 0),
            )
        })
        .collect();
    assert_eq!(
        query.buckets,
        midnights
            .windows(2)
            .map(|pair| UsageMetricsInterval {
                start: pair[0],
                end: pair[1],
            })
            .collect::<Vec<_>>(),
        "29 September to 28 October, each from one New York midnight to the next"
    );
    assert_eq!(
        query.comparison,
        Some(UsageMetricsInterval {
            start: in_new_york(date(2026, 8, 30).at(0, 0, 0, 0)),
            end: in_new_york(date(2026, 9, 28).at(14, 30, 0, 0)),
        }),
        "compared with the thirty New York days before, as far into the last"
    );
    let rows = screen(&application);
    assert!(rows[0].starts_with(" Metrics"), "{rows:#?}");
    row(&rows, "Reading Usage Metrics…");
}

#[test]
fn the_reader_s_own_zone_says_which_day_is_today() {
    // 22:00 on the 27th in New York is already the 28th in Kolkata.
    let now = in_new_york(date(2026, 10, 27).at(22, 0, 0, 0));
    let last_day = |zone_name: &str| {
        let (_, _, query) = ask(&mut reader_in(zone_name, now));
        let last = *query.buckets.last().unwrap();
        assert!(last.contains(now));
        last.start
    };

    let kolkata_midnight = date(2026, 10, 28)
        .at(0, 0, 0, 0)
        .to_zoned(zone("Asia/Kolkata"))
        .unwrap();
    assert_eq!(
        last_day("Asia/Kolkata"),
        SessionTimestamp(kolkata_midnight.timestamp().as_millisecond() as u64)
    );
    assert_eq!(
        last_day(NEW_YORK),
        in_new_york(date(2026, 10, 27).at(0, 0, 0, 0))
    );
}

#[test]
fn a_report_draws_stacked_bars_over_a_nice_axis_with_a_legend() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    show_tokens(&mut application);

    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    assert!(rows[0].trim_end().ends_with("esc ✕"), "{shown}");
    let controls = row(&rows, "by ▐Model▌");
    assert!(
        controls.starts_with("  Today   7 days  ▐30 days▌  Year   All time"),
        "the range bar, thirty days chosen: {controls:?}"
    );

    // The busiest day holds 6.25M, so the axis climbs in steps of 1M to 7M.
    for tick in ["1M", "2M", "7M"] {
        assert!(
            rows.iter()
                .any(|row| row.trim_start().starts_with(&format!("{tick} ┤"))),
            "a tick at {tick}: {shown}"
        );
    }
    assert!(!shown.contains("8M ┤"), "{shown}");

    // A week apart, counting back from today, so none collide.
    let dates = rows
        .iter()
        .find(|row| row.contains("28 Oct"))
        .expect("the x-axis labels today");
    let named: Vec<&str> = ["30 Sep", "7 Oct", "14 Oct", "21 Oct", "28 Oct"]
        .into_iter()
        .filter(|label| dates.contains(label))
        .collect();
    assert_eq!(named.len(), 5, "{dates}");
    let words: Vec<&str> = dates.split_whitespace().collect();
    assert_eq!(words.len(), 10, "only those five: {dates}");
    let at = |label: &str| dates.find(label).unwrap();
    assert!(at("30 Sep") < at("7 Oct") && at("21 Oct") < at("28 Oct"));

    // Largest first, each with its range total and share.
    let legend: Vec<&str> = ["Claude Opus 5.5", "GPT-5.5 Codex", "copilot · GPT-5.5"]
        .into_iter()
        .map(|label| row(&rows, label))
        .collect();
    row(&rows, "Last 30 days · 109M tokens");
    for (line, figures) in legend
        .iter()
        .zip([["90M", "83%"], ["15M", "14%"], ["3.75M", "3%"]])
    {
        for figure in figures {
            assert!(line.contains(figure), "{figure} in {line:?}");
        }
    }
    let legend_at = |label: &str| rendered_row(&rows, label);
    assert!(legend_at("Claude Opus 5.5") < legend_at("GPT-5.5 Codex"));
    assert!(legend_at("GPT-5.5 Codex") < legend_at("copilot · GPT-5.5"));
}

fn rendered_row(rows: &[String], needle: &str) -> usize {
    rows.iter().position(|row| row.contains(needle)).unwrap()
}

#[test]
fn each_bar_stacks_its_models_largest_first_in_theme_colours_to_an_eighth_of_a_row() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    show_tokens(&mut application);

    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (axis_x, axis_y) = text_position(&buffer, "└");
    let swatch = |label: &str| {
        let (x, y) = text_position(&buffer, label);
        buffer[(x - 2, y)].fg
    };
    let (opus, codex, copilot) = (
        swatch("Claude Opus 5.5"),
        swatch("GPT-5.5 Codex"),
        swatch("copilot · GPT-5.5"),
    );
    assert!(
        opus != codex && codex != copilot && opus != copilot,
        "each Model its own colour: {opus:?} {codex:?} {copilot:?}"
    );
    assert!(![opus, codex, copilot].contains(&Color::Reset));

    // Thirty bars share 112 columns: three apiece, two of bar and one of gap.
    let column = |day: u16| axis_x + 1 + day * 3 + 1;
    let cells = |day: u16| {
        (0..axis_y)
            .rev()
            .map(|y| &buffer[(column(day), y)])
            .take_while(|cell| cell.symbol() != " ")
            .collect::<Vec<_>>()
    };

    // The fifteenth day: Opus 5M, Codex 1M, Copilot 250K, over 21 rows of 7M,
    // so 15 rows of Opus, 3 of Codex, and six eighths of one of Copilot.
    let busiest = cells(14);
    assert_eq!(busiest.len(), 19, "{busiest:?}");
    assert!(
        busiest[..15]
            .iter()
            .all(|cell| cell.fg == opus && cell.symbol() == "█")
    );
    assert!(
        busiest[15..18]
            .iter()
            .all(|cell| cell.fg == codex && cell.symbol() == "█")
    );
    assert_eq!(busiest[18].symbol(), "▆");
    assert_eq!(busiest[18].fg, copilot);

    // The first day: Opus 1M then Copilot 250K, three rows and six eighths;
    // the boundary between them falls inside no cell.
    let first = cells(0);
    assert_eq!(first.len(), 4, "{first:?}");
    assert!(first[..3].iter().all(|cell| cell.fg == opus));
    assert_eq!((first[3].symbol(), first[3].fg), ("▆", copilot));

    // The second day: Opus 2M and Codex 500K, which ends halfway up a row,
    // drawn in Codex's colour over the canvas.
    let half = cells(1);
    assert_eq!(half.len(), 8, "{half:?}");
    assert!(half[..6].iter().all(|cell| cell.fg == opus));
    assert!(half[6].fg == codex && half[6].symbol() == "█");
    assert_eq!((half[7].symbol(), half[7].fg), ("▄", codex));
}

#[test]
fn colours_follow_the_series_order_not_the_order_the_report_lists_them_in() {
    let colours = |reverse: bool| {
        let mut application = reader_in(NEW_YORK, afternoon());
        let (_, request_id, query) = ask(&mut application);
        let mut report = report(&query);
        if reverse {
            report.keys.reverse();
            for bucket in &mut report.buckets {
                bucket.by_key.reverse();
            }
        }
        answer(&mut application, request_id, Ok(report));
        show_tokens(&mut application);
        let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
        ["Claude Opus 5.5", "GPT-5.5 Codex", "copilot · GPT-5.5"].map(|label| {
            let (x, y) = text_position(&buffer, label);
            buffer[(x - 2, y)].fg
        })
    };

    assert_eq!(colours(false), colours(true));
}

#[test]
fn a_range_without_usage_says_so() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Ok(UsageMetricsReport {
            buckets: vec![UsageMetricsBucket::default(); query.buckets.len()],
            untimed: UsageMetricsBucket::default(),
            across_buckets_and_untimed: UsageMetricsAggregate::default(),
            comparison: None,
            earliest_turn_at: None,
            keys: Vec::new(),
            across_buckets: UsageMetricsBucket::default(),
        }),
    );

    let rows = screen(&application);
    row(&rows, "No Usage in the last 30 days.");
    assert!(!rows.join("\n").contains('└'), "no empty chart is drawn");
}

#[test]
fn a_refused_read_shows_its_reason() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, _) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Err(UsageMetricsReadRefusal::Refused(
            "A Usage Metrics query's buckets overlap".to_owned(),
        )),
    );

    let rows = screen(&application);
    row(&rows, "The Server refused the Usage Metrics read");
    row(&rows, "A Usage Metrics query's buckets overlap");
}

#[test]
fn a_read_that_never_reached_a_server_says_so() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, _) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Err(UsageMetricsReadRefusal::Failed(
            "send Usage Metrics read command: connection refused".to_owned(),
        )),
    );

    let rows = screen(&application);
    row(&rows, "Usage Metrics could not be read");
    row(&rows, "connection refused");
}

#[test]
fn a_refused_refresh_keeps_the_figures_and_says_why_above_them() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, first, query) = ask(&mut application);
    answer(&mut application, first, Ok(report(&query)));

    let (_, second, _) = expect_read(key(&mut application, KeyCode::Char('r')));
    answer(
        &mut application,
        second,
        Err(UsageMetricsReadRefusal::Failed(
            "connection refused".to_owned(),
        )),
    );

    let rows = screen(&application);
    row(&rows, "Claude Opus 5.5");
    let controls = rendered_row(&rows, "by ▐Model▌");
    let note = rendered_row(&rows, "Could not refresh: connection refused");
    assert_eq!(note, controls + 1, "beneath the controls");
    assert!(!rows.join("\n").contains("Refreshing…"));
    // Its own row, so even 80 columns say all of it.
    let narrow = rendered_application_rows_at(&application, 80, HEIGHT);
    row(&narrow, "Could not refresh: connection refused");

    // A refresh that lands clears the note.
    let (_, third, _) = expect_read(key(&mut application, KeyCode::Char('r')));
    answer(&mut application, third, Ok(report(&query)));
    assert!(
        !screen(&application)
            .join("\n")
            .contains("Could not refresh")
    );
}

#[test]
fn a_wide_label_leaves_the_legend_s_figures_aligned() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = report(&query);
    report.keys[1].label = "模型 Codex".to_owned();
    answer(&mut application, request_id, Ok(report));
    show_tokens(&mut application);

    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let column = |figure: &str| {
        let (_, y) = text_position(&buffer, figure);
        // Read as cells, so a wide glyph counts for the two it covers.
        (0..WIDTH)
            .find(|x| {
                (0..figure.chars().count() as u16)
                    .map(|offset| buffer[(x + offset, y)].symbol())
                    .collect::<String>()
                    == figure
            })
            .unwrap()
    };
    let shares = ["83%", "14%", "3%"].map(|share| column(share) + share.len() as u16);
    assert!(
        shares.iter().all(|end| *end == shares[0]),
        "every share ends in one column: {shares:?}"
    );
}

#[test]
fn ctrl_c_leaves_suru_from_the_view_without_touching_the_draft_it_covers() {
    let (mut application, _workspace) = beside_a_session();
    type_terminal_text(&mut application, "half a thought");
    invoke(&mut application, SemanticCommandId::MetricsShow);

    let quit = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+C");

    assert_eq!(quit, ApplicationTransition::Exit);
}

#[test]
fn the_reader_can_leave_while_the_reconnect_modal_stands_over_the_view() {
    let (mut application, _workspace) = beside_a_session();
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Recovering(
            RecoveryStatus {
                attempt: 1,
                retry_in: std::time::Duration::from_millis(50),
                unreachable: None,
            },
        )))
        .expect("begin recovery");
    grace_elapses(&mut application, Outlook::Local);
    let rows = screen(&application).join("\n");
    assert!(rows.contains("Reconnecting to Suru"), "{rows}");

    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue,
        "only leaving is answered under the modal"
    );
    assert!(screen(&application).join("\n").contains("Metrics"));
    let quit = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+C");
    assert_eq!(quit, ApplicationTransition::Exit);
}

/// An Application with a Session open, so the Sidebar and the Aside both
/// stand beside the main view.
fn beside_a_session() -> (Application, crate::tui_rendering::support::WorkspaceDir) {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path())
        .with_session_clock(afternoon)
        .with_time_zone(zone(NEW_YORK));
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Shown;
    deliver_settings(&mut application, settings);
    enter_session(&mut application, workspace.path());
    (application, workspace)
}

/// The most upright rules any one row draws.
fn rules_in_a_row(rows: &[String]) -> usize {
    rows.iter()
        .map(|row| row.matches('│').count())
        .max()
        .unwrap_or(0)
}

#[test]
fn the_view_covers_the_sidebar_and_the_aside() {
    // Wide enough for the Sidebar and the Aside both to stand.
    let wide = |application: &Application| rendered_application_rows_at(application, 200, HEIGHT);
    let (mut application, _workspace) = beside_a_session();
    let beneath = wide(&application);
    assert!(
        beneath.join("\n").contains("Subagents") && rules_in_a_row(&beneath) >= 2,
        "the Sidebar's rule and the Aside's: {beneath:#?}"
    );

    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));

    let covered = wide(&application);
    let text = covered.join("\n");
    assert!(covered[0].starts_with(" Metrics"), "{text}");
    assert!(!text.contains("Subagents"), "no Aside: {text}");
    assert_eq!(
        rules_in_a_row(&covered),
        1,
        "only the y-axis stands upright: {text}"
    );
}

#[test]
fn esc_or_the_close_control_return_to_exactly_the_screen_beneath() {
    let (mut application, _workspace) = beside_a_session();
    let beneath = screen(&application);

    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    assert_ne!(screen(&application), beneath);
    key(&mut application, KeyCode::Esc);
    assert_eq!(screen(&application), beneath, "Esc closes the view");

    ask(&mut application);
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let close = text_position(&buffer, "✕");
    press_at(&mut application, close);
    assert_eq!(screen(&application), beneath, "so does a press on its ✕");
}

#[test]
fn the_view_opened_by_its_command_leaves_a_draft_where_it_was() {
    let (mut application, _workspace) = beside_a_session();
    type_terminal_text(&mut application, "half a thought");
    let beneath = screen(&application);

    let (_, request_id, query) =
        expect_read(invoke(&mut application, SemanticCommandId::MetricsShow));
    answer(&mut application, request_id, Ok(report(&query)));
    // Keys the view takes reach nothing it covers.
    type_terminal_text(&mut application, "xyz");
    invoke(&mut application, SemanticCommandId::MetricsClose);

    assert_eq!(screen(&application), beneath);
}

#[test]
fn r_reads_again_and_only_the_newest_answer_lands() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, first, query) = ask(&mut application);
    answer(&mut application, first, Ok(report(&query)));

    let (origin, second, again) = expect_read(key(&mut application, KeyCode::Char('r')));
    assert_eq!(origin, Outlook::Local);
    assert_ne!(first, second);
    assert_eq!(again, query, "the same thirty days, read anew");
    let rows = screen(&application);
    row(&rows, "Refreshing…");
    row(&rows, "Claude Opus 5.5");

    // A straggler from the first read moves nothing.
    answer(
        &mut application,
        first,
        Err(UsageMetricsReadRefusal::Failed("too late".to_owned())),
    );
    assert!(!screen(&application).join("\n").contains("too late"));

    let mut quieter = report(&query);
    quieter.buckets = vec![UsageMetricsBucket::default(); query.buckets.len()];
    answer(&mut application, second, Ok(quieter));
    let rows = screen(&application);
    row(&rows, "No Usage in the last 30 days.");
    assert!(!rows.join("\n").contains("Refreshing…"));
}

#[test]
fn a_remote_outlook_reads_the_remote_and_its_silence_is_explained() {
    let mut application = application_looking_at_studio()
        .with_session_clock(afternoon)
        .with_time_zone(zone(NEW_YORK));

    let (origin, _, _) = ask(&mut application);
    assert_eq!(origin, Outlook::Remote("studio".to_owned()));

    studio_stops_answering(&mut application, 1, std::time::Duration::from_secs(5));
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));
    assert_eq!(
        key(&mut application, KeyCode::Char('r')),
        ApplicationTransition::Continue,
        "a refresh waits for the Remote"
    );
    row(&screen(&application), "studio is unreachable");
    key(&mut application, KeyCode::Esc);
    assert!(
        !screen(&application)
            .join("\n")
            .contains("this waits until it answers"),
        "the refusal stayed in the view"
    );

    type_terminal_text(&mut application, "/metrics");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "an Unreachable Origin refuses the view"
    );
    let rows = screen(&application);
    assert!(!rows[0].starts_with(" Metrics"));
    row(&rows, "studio is unreachable");
}

/// Selects the last few characters of the composer's draft from the keys.
fn select_the_draft_s_end(application: &mut Application) {
    for _ in 0..4 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::SHIFT,
            )))
            .expect("extend the selection");
    }
}

#[test]
fn a_selection_left_beneath_the_view_takes_neither_esc_nor_ctrl_c_from_it() {
    let (mut application, _workspace) = beside_a_session();
    type_terminal_text(&mut application, "half a thought");

    select_the_draft_s_end(&mut application);
    invoke(&mut application, SemanticCommandId::MetricsShow);
    key(&mut application, KeyCode::Esc);
    assert!(
        !screen(&application)[0].starts_with(" Metrics"),
        "the first Esc closes the view"
    );

    select_the_draft_s_end(&mut application);
    invoke(&mut application, SemanticCommandId::MetricsShow);
    let quit = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+C");
    assert_eq!(
        quit,
        ApplicationTransition::Exit,
        "Ctrl+C leaves rather than copies"
    );
}

/// The local moment each bucket of `query` begins at in New York.
fn new_york_starts(query: &UsageMetricsQuery) -> Vec<jiff::civil::DateTime> {
    query
        .buckets
        .iter()
        .map(|bucket| {
            jiff::Timestamp::from_millisecond(bucket.start.0 as i64)
                .unwrap()
                .to_zoned(zone(NEW_YORK))
                .datetime()
        })
        .collect()
}

fn contiguous(query: &UsageMetricsQuery) -> bool {
    query
        .buckets
        .windows(2)
        .all(|pair| pair[0].end == pair[1].start)
}

fn press(application: &mut Application, character: char) -> ApplicationTransition {
    key(application, KeyCode::Char(character))
}

fn wheel(
    application: &mut Application,
    kind: MouseEventKind,
    (column, row): (u16, u16),
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("turn the wheel")
}

/// A report of `query`'s buckets with nothing in them, naming `earliest` as
/// the earliest timed Turn.
fn empty_report(
    query: &UsageMetricsQuery,
    earliest: Option<SessionTimestamp>,
) -> UsageMetricsReport {
    UsageMetricsReport {
        buckets: vec![UsageMetricsBucket::default(); query.buckets.len()],
        untimed: UsageMetricsBucket::default(),
        across_buckets_and_untimed: UsageMetricsAggregate::default(),
        comparison: None,
        earliest_turn_at: earliest,
        keys: Vec::new(),
        across_buckets: UsageMetricsBucket::default(),
    }
}

#[test]
fn each_range_key_reads_that_range_in_the_reader_s_calendar() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));

    let (_, _, today) = expect_read(press(&mut application, '1'));
    assert_eq!(
        new_york_starts(&today),
        (0..24)
            .map(|hour| date(2026, 10, 28).at(hour, 0, 0, 0))
            .collect::<Vec<_>>(),
        "today's hours, those still to come included"
    );
    assert!(contiguous(&today));
    assert_eq!(
        today.buckets.last().unwrap().end,
        in_new_york(date(2026, 10, 29).at(0, 0, 0, 0))
    );

    let (_, _, week) = expect_read(press(&mut application, '2'));
    assert_eq!(
        new_york_starts(&week),
        (22..=28)
            .map(|day| date(2026, 10, day).at(0, 0, 0, 0))
            .collect::<Vec<_>>()
    );

    let (_, _, month) = expect_read(press(&mut application, '3'));
    assert_eq!(month, query, "thirty days, as the view opened on");

    let (_, _, year) = expect_read(press(&mut application, '4'));
    let mut months = vec![date(2025, 11, 1), date(2025, 12, 1)];
    months.extend((1..=10).map(|month| date(2026, month, 1)));
    assert_eq!(
        new_york_starts(&year),
        months
            .into_iter()
            .map(|first| first.at(0, 0, 0, 0))
            .collect::<Vec<_>>(),
        "this month and the eleven before it, each from its first midnight"
    );
    assert!(contiguous(&year));
    assert_eq!(
        year.buckets[3].end.0 - year.buckets[3].start.0,
        28 * 24 * 3_600_000,
        "February 2026"
    );

    assert_eq!(
        press(&mut application, '4'),
        ApplicationTransition::Continue,
        "the range already shown is not read again"
    );
}

#[test]
fn today_has_as_many_hours_as_daylight_saving_leaves_it() {
    let hours_on = |day: jiff::civil::Date| {
        let mut application = reader_in(NEW_YORK, in_new_york(day.at(12, 0, 0, 0)));
        ask(&mut application);
        let (_, _, today) = expect_read(press(&mut application, '1'));
        assert!(contiguous(&today));
        assert!(
            today
                .buckets
                .iter()
                .all(|hour| hour.end.0 - hour.start.0 == 3_600_000)
        );
        today.buckets.len()
    };

    assert_eq!(hours_on(date(2026, 3, 8)), 23, "New York springs forward");
    assert_eq!(hours_on(date(2026, 11, 1)), 25, "and falls back");
    assert_eq!(hours_on(date(2026, 11, 2)), 24);
}

#[test]
fn all_time_with_no_report_yet_first_reads_when_the_earliest_turn_was() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, thirty_days, _) = ask(&mut application);

    // Chosen before any report has landed, so nothing says where it begins.
    let (_, probe, first) = expect_read(press(&mut application, '5'));
    assert!(
        first.buckets.is_empty() && first.comparison.is_none(),
        "the first read asks only when the earliest Turn was"
    );
    let earliest = in_new_york(date(2025, 6, 14).at(9, 0, 0, 0));

    // The thirty days asked for before are no longer wanted.
    assert_eq!(
        application
            .handle_event(ApplicationEvent::UsageMetricsRead {
                request_id: thirty_days,
                result: Ok(empty_report(&first, Some(earliest))),
            })
            .expect("deliver a straggler"),
        ApplicationTransition::Continue
    );
    row(&screen(&application), "Reading Usage Metrics…");

    let transition = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id: probe,
            result: Ok(empty_report(&first, Some(earliest))),
        })
        .expect("deliver the earliest Turn");
    let (origin, request_id, all_time) = expect_read(transition);
    assert_eq!(origin, Outlook::Local);
    let mut months: Vec<jiff::civil::DateTime> = (6..=12)
        .map(|month| date(2025, month, 1).at(0, 0, 0, 0))
        .collect();
    months.extend((1..=10).map(|month| date(2026, month, 1).at(0, 0, 0, 0)));
    assert_eq!(
        new_york_starts(&all_time),
        months,
        "every month from the earliest Turn's to this one"
    );
    assert_eq!(
        all_time.comparison, None,
        "nothing comes before all of time"
    );
    assert!(contiguous(&all_time));
    row(&screen(&application), "Reading Usage Metrics…");

    answer(&mut application, request_id, Ok(report(&all_time)));
    let rows = screen(&application);
    row(&rows, "▐All time▌");
    row(&rows, "All time · ");
    let axis = row(&rows, "Oct 2026");
    assert!(axis.contains("Jan 2026"), "{axis}");
}

#[test]
fn all_time_begins_where_the_last_report_said_without_asking_again() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let earliest = in_new_york(date(2026, 8, 3).at(9, 0, 0, 0));
    answer(
        &mut application,
        request_id,
        Ok(empty_report(&query, Some(earliest))),
    );

    let (_, _, all_time) = expect_read(press(&mut application, '5'));

    assert_eq!(
        new_york_starts(&all_time),
        [8, 9, 10].map(|month| date(2026, month, 1).at(0, 0, 0, 0))
    );
}

#[test]
fn all_time_reads_again_when_its_report_names_an_earlier_turn() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Ok(empty_report(
            &query,
            Some(in_new_york(date(2026, 8, 3).at(9, 0, 0, 0))),
        )),
    );
    let (_, request_id, all_time) = expect_read(press(&mut application, '5'));

    // A Turn from before the buckets asked for, such as one restored since.
    let earlier = in_new_york(date(2026, 5, 20).at(9, 0, 0, 0));
    let transition = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id,
            result: Ok(empty_report(&all_time, Some(earlier))),
        })
        .expect("deliver the report");

    let (_, request_id, again) = expect_read(transition);
    assert_eq!(new_york_starts(&again)[0], date(2026, 5, 1).at(0, 0, 0, 0));
    let landed = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id,
            result: Ok(empty_report(&again, Some(earlier))),
        })
        .expect("deliver the report");
    assert_eq!(landed, ApplicationTransition::Continue);
    row(&screen(&application), "No Usage yet.");
}

#[test]
fn all_time_without_a_timed_turn_is_this_month_alone() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    let (_, probe, first) = expect_read(press(&mut application, '5'));

    let transition = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id: probe,
            result: Ok(empty_report(&first, None)),
        })
        .expect("deliver the report");

    let (_, request_id, this_month) = expect_read(transition);
    assert_eq!(
        new_york_starts(&this_month),
        [date(2026, 10, 1).at(0, 0, 0, 0)]
    );
    let landed = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id,
            result: Ok(empty_report(&this_month, None)),
        })
        .expect("deliver the report");
    assert_eq!(landed, ApplicationTransition::Continue);
    row(&screen(&application), "No Usage yet.");
}

#[test]
fn a_range_chosen_while_all_time_learns_its_beginning_drops_that_answer() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    let (_, probe, first) = expect_read(press(&mut application, '5'));
    assert!(first.buckets.is_empty());

    let (_, request_id, week) = expect_read(press(&mut application, '2'));
    let late = application
        .handle_event(ApplicationEvent::UsageMetricsRead {
            request_id: probe,
            result: Ok(empty_report(
                &first,
                Some(in_new_york(date(2025, 6, 14).at(9, 0, 0, 0))),
            )),
        })
        .expect("deliver the late answer");

    assert_eq!(late, ApplicationTransition::Continue, "nothing read again");
    row(&screen(&application), "Reading Usage Metrics…");
    answer(&mut application, request_id, Ok(report(&week)));
    let rows = screen(&application);
    row(&rows, "▐7 days▌");
    row(&rows, "Last 7 days · ");
    row(&rows, "28 Oct");
}

#[test]
fn the_range_outlives_the_view_and_all_time_learns_its_beginning_anew_on_opening() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Ok(empty_report(
            &query,
            Some(in_new_york(date(2026, 8, 3).at(9, 0, 0, 0))),
        )),
    );
    let (_, request_id, all_time) = expect_read(press(&mut application, '5'));
    answer(&mut application, request_id, Ok(report(&all_time)));
    key(&mut application, KeyCode::Esc);

    let (_, _, reopened) = ask(&mut application);

    assert!(
        reopened.buckets.is_empty(),
        "All time still, read from its earliest Turn again"
    );
    row(&screen(&application), "▐All time▌");
}

#[test]
fn brackets_clicks_and_the_wheel_choose_ranges_as_the_keys_do() {
    assert_eq!(
        [
            SemanticCommandId::MetricsRangeSelect(MetricsRange::Today),
            SemanticCommandId::MetricsRangeSelect(MetricsRange::SevenDays),
            SemanticCommandId::MetricsRangeSelect(MetricsRange::ThirtyDays),
            SemanticCommandId::MetricsRangeSelect(MetricsRange::Year),
            SemanticCommandId::MetricsRangeSelect(MetricsRange::AllTime),
            SemanticCommandId::MetricsRangeNext,
            SemanticCommandId::MetricsRangePrevious,
        ]
        .map(SemanticCommandId::as_str),
        [
            "metrics.range.today",
            "metrics.range.seven-days",
            "metrics.range.thirty-days",
            "metrics.range.year",
            "metrics.range.all-time",
            "metrics.range.next",
            "metrics.range.previous",
        ]
    );
    // Each way of asking for a range, from a view showing thirty days.
    let read_by = |act: &dyn Fn(&mut Application) -> ApplicationTransition| {
        let mut application = reader_in(NEW_YORK, afternoon());
        let (_, request_id, query) = ask(&mut application);
        answer(&mut application, request_id, Ok(report(&query)));
        rendered_application_buffer(&application, WIDTH, HEIGHT);
        let (_, _, query) = expect_read(act(&mut application));
        let rows = screen(&application);
        (query.buckets, row(&rows, "by ▐Model▌").to_owned())
    };
    let semantic = |command| move |application: &mut Application| invoke(application, command);
    let chart = |application: &Application| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        let (x, y) = text_position(&buffer, "└");
        (x + 10, y - 3)
    };
    let range_label = |label: &'static str| {
        move |application: &mut Application| {
            let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
            press_at(application, text_position(&buffer, label))
        }
    };

    let year = read_by(&semantic(SemanticCommandId::MetricsRangeSelect(
        MetricsRange::Year,
    )));
    assert!(year.1.contains("▐Year▌"), "{}", year.1);
    assert_eq!(read_by(&|application| press(application, '4')), year);
    assert_eq!(read_by(&|application| press(application, ']')), year);
    assert_eq!(
        read_by(&semantic(SemanticCommandId::MetricsRangeNext)),
        year
    );
    assert_eq!(read_by(&range_label("Year")), year);
    assert_eq!(
        read_by(&|application| {
            let at = chart(application);
            wheel(application, MouseEventKind::ScrollDown, at)
        }),
        year,
        "down the wheel to the longer range"
    );

    let week = read_by(&semantic(SemanticCommandId::MetricsRangeSelect(
        MetricsRange::SevenDays,
    )));
    assert!(week.1.contains("▐7 days▌"), "{}", week.1);
    assert_eq!(read_by(&|application| press(application, '2')), week);
    assert_eq!(read_by(&|application| press(application, '[')), week);
    assert_eq!(read_by(&range_label("7 days")), week);
    assert_eq!(
        read_by(&|application| {
            let at = chart(application);
            wheel(application, MouseEventKind::ScrollUp, at)
        }),
        week
    );

    // The keys and the wheel name the very commands the invocations do.
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let at = chart(&application);
    let command_for = |event| application.command_for_terminal_input(event);
    let wheel_event = |kind| {
        InputEvent::Mouse(MouseEvent {
            kind,
            column: at.0,
            row: at.1,
            modifiers: KeyModifiers::NONE,
        })
    };
    let key_event =
        |character| InputEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    let semantic_command = |command| Some(CommandId::InvokeSemantic(command));
    assert_eq!(
        command_for(wheel_event(MouseEventKind::ScrollDown)),
        semantic_command(SemanticCommandId::MetricsRangeNext)
    );
    assert_eq!(
        command_for(key_event(']')),
        semantic_command(SemanticCommandId::MetricsRangeNext)
    );
    assert_eq!(
        command_for(wheel_event(MouseEventKind::ScrollUp)),
        semantic_command(SemanticCommandId::MetricsRangePrevious)
    );
    assert_eq!(
        command_for(key_event('[')),
        semantic_command(SemanticCommandId::MetricsRangePrevious)
    );
    // Over the title bar the wheel steps nothing.
    let title = InputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 3,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(command_for(title), None);
}

#[test]
fn stepping_stops_at_either_end_of_the_range_bar() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    expect_read(press(&mut application, '1'));
    assert_eq!(
        press(&mut application, '['),
        ApplicationTransition::Continue
    );
    expect_read(press(&mut application, '5'));
    assert_eq!(
        press(&mut application, ']'),
        ApplicationTransition::Continue
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::MetricsRangeNext),
        ApplicationTransition::Continue
    );
}

#[test]
fn a_range_chosen_with_the_view_closed_changes_nothing() {
    let mut application = reader_in(NEW_YORK, afternoon());

    assert_eq!(
        invoke(
            &mut application,
            SemanticCommandId::MetricsRangeSelect(MetricsRange::Year)
        ),
        ApplicationTransition::Continue
    );
    let (_, _, query) = ask(&mut application);
    assert_eq!(query.buckets.len(), 30, "still thirty days");
}

#[test]
fn today_is_labelled_by_the_hour_and_a_year_by_the_month() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    let (_, request_id, today) = expect_read(press(&mut application, '1'));
    answer(&mut application, request_id, Ok(report(&today)));
    let rows = screen(&application);
    eprintln!("{}", rows.join("\n"));
    let hours = row(&rows, "00:00");
    let labels: Vec<&str> = hours.split_whitespace().collect();
    assert_eq!(
        labels,
        (0..12)
            .map(|hour| format!("{:02}:00", hour * 2))
            .collect::<Vec<_>>(),
        "every other hour from midnight: {hours}"
    );
    row(&rows, "Today · ");
    row(&rows, "▐Today▌");

    let (_, request_id, year) = expect_read(press(&mut application, '4'));
    answer(&mut application, request_id, Ok(report(&year)));
    let rows = screen(&application);
    eprintln!("{}", rows.join("\n"));
    let months = row(&rows, "Nov 2025");
    let labels: Vec<&str> = months.split_whitespace().collect();
    assert_eq!(labels.len(), 24, "every month named: {months}");
    assert_eq!(labels.last(), Some(&"2026"));
    assert!(months.contains("Oct 2026"));
    row(&rows, "Last 12 months · ");
}

#[test]
fn a_range_chosen_while_the_remote_is_unreachable_is_refused_in_the_view() {
    let mut application = application_looking_at_studio()
        .with_session_clock(afternoon)
        .with_time_zone(zone(NEW_YORK));
    ask(&mut application);
    studio_stops_answering(&mut application, 1, std::time::Duration::from_secs(5));
    grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));

    assert_eq!(
        press(&mut application, '2'),
        ApplicationTransition::Continue
    );

    let rows = screen(&application);
    row(&rows, "▐7 days▌");
    row(&rows, "studio is unreachable");
}

/// `report`'s thirty days, priced: Opus at a dollar for each million tokens
/// it took, Codex at two, all of it Estimated, and Copilot not at all, so
/// every day it worked leaves its figures partial. Over the range that is
/// $90 of Opus and $30 of Codex, and the busiest day holds $7.
fn priced_report(query: &UsageMetricsQuery) -> UsageMetricsReport {
    let mut report = report(query);
    for bucket in &mut report.buckets {
        let mut total = UsageMetricsAggregate {
            cost: None,
            ..bucket.total
        };
        for keyed in &mut bucket.by_key {
            let tokens = keyed.aggregate.fresh_input_tokens.unwrap_or(0);
            let aggregate = &mut keyed.aggregate;
            if keyed.key == opus() {
                aggregate.cost = Some(Cost::from_nano_usd(tokens * 1_000));
            } else if keyed.key == codex() {
                aggregate.cost = Some(Cost::from_nano_usd(tokens * 2_000));
                aggregate.estimated_cost = aggregate.cost;
            } else {
                aggregate.is_cost_partial = tokens > 0;
            }
            if let Some(cost) = aggregate.cost {
                total.cost = Some(total.cost.map_or(cost, |held| held.saturating_add(cost)));
            }
            total.is_cost_partial |= aggregate.is_cost_partial;
        }
        bucket.total = total;
    }
    take_together(&mut report);
    report
}

#[test]
fn cost_is_shown_first_in_dollars_with_a_plus_after_every_partial_figure() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));

    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    let controls = row(&rows, "by ▐Model▌");
    assert!(
        controls.contains("▐Cost▌  Tokens"),
        "Cost chosen beside the range bar: {controls:?}"
    );
    // The busiest day holds $7, so the axis climbs a dollar a tick.
    for tick in ["$1.00", "$7.00"] {
        assert!(
            rows.iter()
                .any(|row| row.trim_start().starts_with(&format!("{tick} ┤"))),
            "a tick at {tick}: {shown}"
        );
    }
    assert!(!shown.contains("$8.00 ┤"), "{shown}");
    row(&rows, "Last 30 days · $120.00+");
    for (label, figures) in [
        ("Claude Opus 5.5", ["$90.00 ", "75%"]),
        ("GPT-5.5 Codex", ["$30.00 ", "25%"]),
        ("copilot · GPT-5.5", ["$0.00+", "0%"]),
    ] {
        let line = row(&rows, label);
        for figure in figures {
            assert!(line.contains(figure), "{figure} in {line:?}");
        }
    }
    let legend_at = |label: &str| rendered_row(&rows, label);
    assert!(legend_at("Claude Opus 5.5") < legend_at("GPT-5.5 Codex"));
    assert!(legend_at("GPT-5.5 Codex") < legend_at("copilot · GPT-5.5"));
}

#[test]
fn work_with_no_known_cost_is_not_shown_as_no_usage() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = empty_report(&query, None);
    report.keys = vec![UsageMetricsKeyLabel {
        key: copilot(),
        label: "copilot · GPT-5.5".to_owned(),
    }];
    let unpriced = UsageMetricsAggregate {
        is_cost_partial: true,
        ..tokens(1_000)
    };
    report.buckets[29] = UsageMetricsBucket {
        total: unpriced,
        by_key: vec![KeyedUsageMetrics {
            key: copilot(),
            aggregate: unpriced,
        }],
    };
    take_together(&mut report);
    answer(&mut application, request_id, Ok(report));

    let rows = screen(&application);
    assert!(!rows.join("\n").contains("No Usage"), "{rows:#?}");
    row(&rows, "Last 30 days · $0.00+");
}

#[test]
fn measure_keys_tab_and_clicks_choose_a_measure_without_reading_again() {
    assert_eq!(
        [
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Cost),
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Tokens),
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Sessions),
            SemanticCommandId::MetricsMeasureNext,
        ]
        .map(SemanticCommandId::as_str),
        [
            "metrics.measure.cost",
            "metrics.measure.tokens",
            "metrics.measure.sessions",
            "metrics.measure.next",
        ]
    );
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));
    let shows = |application: &Application, chosen: &str, total: &str| {
        let rows = screen(application);
        let controls = row(&rows, "by ▐Model▌");
        assert!(controls.contains(chosen), "{chosen} in {controls:?}");
        row(&rows, total);
    };
    let tokens = ("▐Tokens▌", "Last 30 days · 109M tokens");
    let cost = ("▐Cost▌", "Last 30 days · $120.00+");
    let sessions = ("▐Sessions▌", "Last 30 days · 15 Sessions");
    shows(&application, cost.0, cost.1);
    let label_at = |application: &Application, label: &str| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        let (_, controls_y) = text_position(&buffer, "by ▐Model▌");
        let rows = screen(application);
        let x = rows[usize::from(controls_y)].find(label).unwrap();
        // The row holds wide glyphs before the label: count its columns.
        let x = unicode_width::UnicodeWidthStr::width(&rows[usize::from(controls_y)][..x]);
        (u16::try_from(x).unwrap(), controls_y)
    };

    // Every way of choosing a measure shows it at once, reading nothing.
    for (act, expected) in [
        (
            Box::new(|application: &mut Application| press(application, 't'))
                as Box<dyn Fn(&mut Application) -> ApplicationTransition>,
            tokens,
        ),
        (
            Box::new(|application: &mut Application| press(application, 'c')),
            cost,
        ),
        (
            Box::new(|application: &mut Application| press(application, 's')),
            sessions,
        ),
        (
            Box::new(|application: &mut Application| press(application, 'c')),
            cost,
        ),
        (
            Box::new(|application: &mut Application| key(application, KeyCode::Tab)),
            tokens,
        ),
        (
            Box::new(|application: &mut Application| key(application, KeyCode::Tab)),
            sessions,
        ),
        (
            Box::new(|application: &mut Application| key(application, KeyCode::Tab)),
            cost,
        ),
        (
            Box::new(|application: &mut Application| {
                let at = label_at(application, "Sessions");
                press_at(application, at)
            }),
            sessions,
        ),
        (
            Box::new(|application: &mut Application| {
                let at = label_at(application, "Tokens");
                press_at(application, at)
            }),
            tokens,
        ),
        (
            Box::new(|application: &mut Application| {
                let at = label_at(application, "Cost");
                press_at(application, at)
            }),
            cost,
        ),
        (
            Box::new(|application: &mut Application| {
                invoke(
                    application,
                    SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Tokens),
                )
            }),
            tokens,
        ),
        (
            Box::new(|application: &mut Application| {
                invoke(application, SemanticCommandId::MetricsMeasureNext)
            }),
            sessions,
        ),
        (
            Box::new(|application: &mut Application| {
                invoke(
                    application,
                    SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Cost),
                )
            }),
            cost,
        ),
    ] {
        assert_eq!(act(&mut application), ApplicationTransition::Continue);
        shows(&application, expected.0, expected.1);
    }

    // The keys name the very commands the invocations do; a click is
    // resolved where it is released, and lands as they do above.
    let command_for = |event| application.command_for_terminal_input(event);
    let semantic_command = |command| Some(CommandId::InvokeSemantic(command));
    let key_event = |code| InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    assert_eq!(
        command_for(key_event(KeyCode::Char('c'))),
        semantic_command(SemanticCommandId::MetricsMeasureSelect(
            MetricsMeasure::Cost
        ))
    );
    assert_eq!(
        command_for(key_event(KeyCode::Char('t'))),
        semantic_command(SemanticCommandId::MetricsMeasureSelect(
            MetricsMeasure::Tokens
        ))
    );
    assert_eq!(
        command_for(key_event(KeyCode::Char('s'))),
        semantic_command(SemanticCommandId::MetricsMeasureSelect(
            MetricsMeasure::Sessions
        ))
    );
    assert_eq!(
        command_for(key_event(KeyCode::Tab)),
        semantic_command(SemanticCommandId::MetricsMeasureNext)
    );
}

/// The note the legend carries where its series count more Sessions than
/// worked in all.
const OVERLAP_NOTE: &str = "A Session that used several Models counts once for each";

#[test]
fn sessions_plot_each_model_s_sessions_over_the_distinct_total_with_a_note() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    assert_eq!(
        press(&mut application, 's'),
        ApplicationTransition::Continue
    );

    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    // The busiest days stack four Sessions: a tick for each.
    for tick in ["1", "4"] {
        assert!(
            rows.iter()
                .any(|row| row.trim_start().starts_with(&format!("{tick} ┤"))),
            "a tick at {tick}: {shown}"
        );
    }
    assert!(!shown.contains("5 ┤"), "{shown}");
    // The heading counts each Session once; each Model counts those that
    // used it, their share of all the Sessions that worked.
    row(&rows, "Last 30 days · 15 Sessions");
    for (label, figures) in [
        ("Claude Opus 5.5", [" 12 ", "80%"]),
        ("GPT-5.5 Codex", [" 4 ", "27%"]),
        ("copilot · GPT-5.5", [" 3 ", "20%"]),
    ] {
        let line = row(&rows, label);
        for figure in figures {
            assert!(line.contains(figure), "{figure} in {line:?}");
        }
    }
    let legend_at = |label: &str| rendered_row(&rows, label);
    assert_eq!(
        legend_at(OVERLAP_NOTE),
        legend_at("copilot · GPT-5.5") + 1,
        "the note stands under the legend: {shown}"
    );

    // Cost and Tokens add up across Models, so they carry no such note.
    show_tokens(&mut application);
    assert!(!screen(&application).join("\n").contains(OVERLAP_NOTE));
}

#[test]
fn sessions_that_each_used_one_model_carry_no_note() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = report(&query);
    // Every Session used one Model alone: the Models add up to the total.
    report.across_buckets.total.sessions = 19;
    answer(&mut application, request_id, Ok(report));
    press(&mut application, 's');

    let rows = screen(&application);
    row(&rows, "Last 30 days · 19 Sessions");
    assert!(!rows.join("\n").contains(OVERLAP_NOTE), "{rows:#?}");
}

#[test]
fn one_session_is_named_in_the_singular() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = empty_report(&query, None);
    report.keys = vec![UsageMetricsKeyLabel {
        key: opus(),
        label: "Claude Opus 5.5".to_owned(),
    }];
    let one = UsageMetricsAggregate {
        sessions: 1,
        subagent_sessions: 3,
        ..tokens(1_000)
    };
    let worked = UsageMetricsBucket {
        total: one,
        by_key: vec![KeyedUsageMetrics {
            key: opus(),
            aggregate: one,
        }],
    };
    report.buckets[29] = worked.clone();
    report.across_buckets = worked;
    answer(&mut application, request_id, Ok(report));
    press(&mut application, 's');

    let rows = screen(&application);
    row(&rows, "Last 30 days · 1 Session");
    assert!(
        !rows.join("\n").contains("1 Sessions"),
        "Subagents are not counted among the Sessions: {rows:#?}"
    );
}

#[test]
fn the_measure_outlives_the_view_and_is_chosen_while_a_read_is_on_its_way() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    // Chosen before the report lands, it reads nothing, and the report
    // still lands under the read asked for.
    show_tokens(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));
    row(&screen(&application), "Last 30 days · 109M tokens");

    key(&mut application, KeyCode::Esc);
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));
    row(&screen(&application), "Last 30 days · 109M tokens");
}

fn provider(provider: &str) -> UsageMetricsKey {
    UsageMetricsKey::Provider {
        provider: ProviderId::new(provider),
    }
}

/// [`report`]'s figures under the Providers its Models are reached through,
/// each Provider having the one Model, as the Server would break them down.
fn by_provider(query: &UsageMetricsQuery) -> UsageMetricsReport {
    let mut report = report(query);
    let renamed = |key: &mut UsageMetricsKey| {
        if let UsageMetricsKey::Model { provider: id, .. } = key {
            *key = provider(id.as_str());
        }
    };
    for label in &mut report.keys {
        renamed(&mut label.key);
        label.label = match label.label.as_str() {
            "Claude Opus 5.5" => "Claude",
            "GPT-5.5 Codex" => "Codex",
            _ => "Copilot",
        }
        .to_owned();
    }
    for bucket in report
        .buckets
        .iter_mut()
        .chain([&mut report.across_buckets])
    {
        bucket
            .by_key
            .iter_mut()
            .for_each(|keyed| renamed(&mut keyed.key));
    }
    report
}

#[test]
fn b_and_clicks_choose_a_breakdown_and_read_it_anew() {
    assert_eq!(
        [
            SemanticCommandId::MetricsBreakdownSelect(UsageMetricsBreakdown::Model),
            SemanticCommandId::MetricsBreakdownSelect(UsageMetricsBreakdown::Provider),
            SemanticCommandId::MetricsBreakdownSelect(UsageMetricsBreakdown::Workspace),
            SemanticCommandId::MetricsBreakdownNext,
        ]
        .map(SemanticCommandId::as_str),
        [
            "metrics.breakdown.model",
            "metrics.breakdown.provider",
            "metrics.breakdown.workspace",
            "metrics.breakdown.next",
        ]
    );
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, opened) = ask(&mut application);
    assert_eq!(opened.breakdown, UsageMetricsBreakdown::Model);
    answer(&mut application, request_id, Ok(report(&opened)));
    let controls = |application: &Application| row(&screen(application), "Sessions").to_owned();
    assert!(
        controls(&application).contains("by ▐Model▌  Provider   Workspace"),
        "{}",
        controls(&application)
    );
    let click = |application: &mut Application, label: &str| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        let (_, controls_y) = text_position(&buffer, "▐Cost▌");
        let rows = screen(application);
        let x = rows[usize::from(controls_y)].find(label).unwrap();
        let x = unicode_width::UnicodeWidthStr::width(&rows[usize::from(controls_y)][..x]);
        press_at(application, (u16::try_from(x).unwrap(), controls_y))
    };

    // `b` steps through every breakdown and back, each step a read of the
    // same buckets split anew; a click or an invocation chooses one.
    for (act, expected) in [
        (
            Box::new(|application: &mut Application| press(application, 'b'))
                as Box<dyn Fn(&mut Application) -> ApplicationTransition>,
            UsageMetricsBreakdown::Provider,
        ),
        (
            Box::new(|application: &mut Application| press(application, 'b')),
            UsageMetricsBreakdown::Workspace,
        ),
        (
            Box::new(|application: &mut Application| press(application, 'b')),
            UsageMetricsBreakdown::Model,
        ),
        (
            Box::new(|application: &mut Application| click(application, "Workspace")),
            UsageMetricsBreakdown::Workspace,
        ),
        (
            Box::new(|application: &mut Application| click(application, "Provider")),
            UsageMetricsBreakdown::Provider,
        ),
        (
            Box::new(|application: &mut Application| {
                invoke(
                    application,
                    SemanticCommandId::MetricsBreakdownSelect(UsageMetricsBreakdown::Model),
                )
            }),
            UsageMetricsBreakdown::Model,
        ),
        (
            Box::new(|application: &mut Application| {
                invoke(application, SemanticCommandId::MetricsBreakdownNext)
            }),
            UsageMetricsBreakdown::Provider,
        ),
    ] {
        let (_, request_id, query) = expect_read(act(&mut application));
        assert_eq!(query.breakdown, expected);
        assert_eq!(query.buckets, opened.buckets);
        // Figures split another way are not shown under the new name.
        assert!(
            screen(&application)
                .iter()
                .any(|row| row.contains("Reading Usage Metrics")),
            "{expected:?}"
        );
        let chosen = match expected {
            UsageMetricsBreakdown::Model => "▐Model▌",
            UsageMetricsBreakdown::Provider => "▐Provider▌",
            UsageMetricsBreakdown::Workspace => "▐Workspace▌",
        };
        assert!(controls(&application).contains(chosen), "{chosen}");
        answer(&mut application, request_id, Ok(empty_report(&query, None)));
    }

    // Choosing the breakdown already shown reads nothing.
    assert_eq!(
        click(&mut application, "Provider"),
        ApplicationTransition::Continue
    );

    // The key names the very command the invocation does.
    assert_eq!(
        application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE
        ))),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsBreakdownNext
        ))
    );
}

#[test]
fn the_breakdown_outlives_the_view() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    expect_read(press(&mut application, 'b'));
    key(&mut application, KeyCode::Esc);

    let (_, _, query) = ask(&mut application);

    assert_eq!(query.breakdown, UsageMetricsBreakdown::Provider);
}

#[test]
fn a_provider_breakdown_names_each_provider_and_notes_sessions_counted_under_several() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    answer(&mut application, request_id, Ok(by_provider(&query)));
    press(&mut application, 's');

    let rows = screen(&application);
    for (label, figure) in [("Claude", " 12 "), ("Codex", " 4 "), ("Copilot", " 3 ")] {
        let line = row(&rows, &format!("█ {label}"));
        assert!(line.contains(figure), "{figure} in {line:?}");
    }
    row(
        &rows,
        "A Session that used several Providers counts once for each",
    );
    assert!(!rows.join("\n").contains("several Models"));
}

/// Thirty days of `keys` Workspaces, the first using a million tokens a day
/// and each after it a hundred thousand fewer, every one of them the work of
/// one Session alone.
fn many_workspaces(query: &UsageMetricsQuery, keys: u64) -> UsageMetricsReport {
    let workspace = |index: u64| UsageMetricsKey::Workspace {
        workspace: suru::protocol::WorkspaceId(format!("directory:{index}")),
    };
    let daily = |index: u64| 1_000_000 - index * 100_000;
    let keyed = |index: u64| KeyedUsageMetrics {
        key: workspace(index),
        aggregate: UsageMetricsAggregate {
            sessions: 1,
            ..tokens(daily(index))
        },
    };
    let mut report = UsageMetricsReport {
        buckets: query
            .buckets
            .iter()
            .map(|_| UsageMetricsBucket {
                total: UsageMetricsAggregate {
                    sessions: keys,
                    ..tokens((0..keys).map(daily).sum())
                },
                by_key: (0..keys).map(keyed).collect(),
            })
            .collect(),
        untimed: UsageMetricsBucket::default(),
        across_buckets_and_untimed: UsageMetricsAggregate::default(),
        comparison: None,
        earliest_turn_at: query.buckets.first().map(|bucket| bucket.start),
        // By identity, which says nothing of their size.
        keys: (0..keys)
            .rev()
            .map(|index| UsageMetricsKeyLabel {
                key: workspace(index),
                label: format!("workspace-{index}"),
            })
            .collect(),
        across_buckets: UsageMetricsBucket {
            total: UsageMetricsAggregate {
                sessions: keys,
                ..UsageMetricsAggregate::default()
            },
            by_key: (0..keys).map(keyed).collect(),
        },
    };
    for keyed in &mut report.across_buckets.by_key {
        keyed.aggregate = UsageMetricsAggregate {
            sessions: 1,
            ..UsageMetricsAggregate::default()
        };
    }
    take_together(&mut report);
    report
}

#[test]
fn keys_beyond_the_six_largest_fold_into_other_with_their_figures_added() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    expect_read(press(&mut application, 'b'));
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    answer(&mut application, request_id, Ok(many_workspaces(&query, 9)));
    show_tokens(&mut application);

    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    for index in 0..6 {
        row(&rows, &format!("workspace-{index}"));
    }
    for index in 6..9 {
        assert!(!shown.contains(&format!("workspace-{index}")), "{shown}");
    }
    // The seventh to ninth use 400K, 300K and 200K a day: 27M over thirty.
    let other = row(&rows, "█ Other");
    assert!(other.contains("27M"), "{other:?}");
    assert!(other.contains("17%"), "27M of 162M: {other:?}");
    assert_eq!(
        rendered_row(&rows, "█ Other"),
        rendered_row(&rows, "workspace-5") + 1,
        "Other stands after the six it is smaller than"
    );
    row(&rows, "Last 30 days · 162M tokens");

    // Each of the six its own colour, and Other one of its own beside them.
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let swatch = |label: &str| {
        let (x, y) = text_position(&buffer, label);
        buffer[(x - 2, y)].fg
    };
    let mut colours = (0..6)
        .map(|index| swatch(&format!("workspace-{index}")))
        .chain([swatch("Other")])
        .collect::<Vec<_>>();
    colours.sort_by_key(|colour| format!("{colour:?}"));
    colours.dedup();
    assert_eq!(colours.len(), 7, "{colours:?}");
}

#[test]
fn six_keys_fold_nothing() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(many_workspaces(&query, 6)));

    let shown = screen(&application).join("\n");
    assert!(shown.contains("workspace-5"), "{shown}");
    assert!(!shown.contains("Other"), "{shown}");
}

#[test]
fn other_counts_the_sessions_of_the_keys_it_holds_once_for_each() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = many_workspaces(&query, 8);
    // One Session worked under every key, so the keys count eight Sessions
    // of the one.
    report.across_buckets.total.sessions = 1;
    answer(&mut application, request_id, Ok(report));
    press(&mut application, 's');

    let rows = screen(&application);
    row(&rows, "Last 30 days · 1 Session");
    let other = row(&rows, "█ Other");
    assert!(other.contains(" 2 "), "{other:?}");
    row(
        &rows,
        "A Session that used several Models counts once for each",
    );
}

#[test]
fn each_series_keeps_its_colour_whatever_the_measure() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut report = priced_report(&query);
    // Copilot's few tokens cost the most, so Cost and Tokens rank the
    // Models differently.
    for bucket in &mut report.buckets {
        for keyed in &mut bucket.by_key {
            if keyed.key == copilot() {
                keyed.aggregate.cost = Some(Cost::from_nano_usd(100_000_000_000));
            }
        }
    }
    take_together(&mut report);
    answer(&mut application, request_id, Ok(report));
    let colours = |application: &Application| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        ["Claude Opus 5.5", "GPT-5.5 Codex", "copilot · GPT-5.5"].map(|label| {
            let (x, y) = text_position(&buffer, label);
            buffer[(x - 2, y)].fg
        })
    };

    let by_cost = colours(&application);
    show_tokens(&mut application);
    let by_tokens = colours(&application);
    press(&mut application, 's');

    assert_eq!(by_cost, by_tokens);
    assert_eq!(by_cost, colours(&application));
}

#[test]
fn an_answer_awaited_before_a_choice_the_origin_could_not_read_never_lands() {
    for choose in ['b', '2'] {
        let mut application = application_looking_at_studio()
            .with_session_clock(afternoon)
            .with_time_zone(zone(NEW_YORK));
        let (_, request_id, query) = ask(&mut application);
        studio_stops_answering(&mut application, 1, std::time::Duration::from_secs(5));
        grace_elapses(&mut application, Outlook::Remote("studio".to_owned()));
        assert_eq!(
            press(&mut application, choose),
            ApplicationTransition::Continue
        );

        // The thirty days by Model, asked before the choice, answer late.
        answer(&mut application, request_id, Ok(report(&query)));

        let shown = screen(&application).join("\n");
        assert!(!shown.contains("Claude Opus 5.5"), "{choose}: {shown}");
        assert!(shown.contains("studio is unreachable"), "{choose}: {shown}");
    }
}

/// A view showing thirty days of [`report`] in Tokens, drawn once so its
/// bars can be pointed at.
fn thirty_days_of_tokens() -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    show_tokens(&mut application);
    rendered_application_buffer(&application, WIDTH, HEIGHT);
    application
}

/// Where the bar of the `day`-th bucket stands, counted from the first:
/// thirty bars share 112 columns, three apiece, a column of gap before
/// two of bar. Answers its first column and a row inside the chart.
fn bar_at(application: &Application, day: u16) -> (u16, u16) {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (axis_x, axis_y) = text_position(&buffer, "└");
    (axis_x + 1 + day * 3 + 1, axis_y - 3)
}

/// The legend's heading: the row above its first series.
fn legend_heading(application: &Application) -> String {
    let rows = screen(application);
    let first = rendered_row(&rows, "Claude Opus 5.5");
    rows[first - 1].trim().to_owned()
}

fn drag_to(application: &mut Application, (column, row): (u16, u16)) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("drag the pointer")
}

fn release_at(application: &mut Application, (column, row): (u16, u16)) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("release the pointer")
}

#[test]
fn arrows_home_and_end_move_a_selection_that_stops_at_either_end() {
    assert_eq!(
        [
            SemanticCommandId::MetricsBucketPrevious,
            SemanticCommandId::MetricsBucketNext,
            SemanticCommandId::MetricsBucketFirst,
            SemanticCommandId::MetricsBucketLast,
            SemanticCommandId::MetricsBucketSelect { bucket: 3 },
            SemanticCommandId::MetricsBucketClear,
        ]
        .map(SemanticCommandId::as_str),
        [
            "metrics.bucket.previous",
            "metrics.bucket.next",
            "metrics.bucket.first",
            "metrics.bucket.last",
            "metrics.bucket.select",
            "metrics.bucket.clear",
        ]
    );
    let mut application = thirty_days_of_tokens();
    let command_for = |application: &Application, code| {
        application
            .command_for_terminal_input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    };
    for (code, command) in [
        (KeyCode::Left, SemanticCommandId::MetricsBucketPrevious),
        (KeyCode::Right, SemanticCommandId::MetricsBucketNext),
        (KeyCode::Home, SemanticCommandId::MetricsBucketFirst),
        (KeyCode::End, SemanticCommandId::MetricsBucketLast),
    ] {
        assert_eq!(
            command_for(&application, code),
            Some(CommandId::InvokeSemantic(command))
        );
    }
    assert!(legend_heading(&application).starts_with("Last 30 days"));

    // ← enters from the right, at today, and stops at it going right.
    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert!(
        legend_heading(&application).starts_with("Wed 28 Oct ·"),
        "{}",
        legend_heading(&application)
    );
    key(&mut application, KeyCode::Right);
    assert!(legend_heading(&application).starts_with("Wed 28 Oct ·"));
    key(&mut application, KeyCode::Left);
    assert!(legend_heading(&application).starts_with("Tue 27 Oct ·"));

    // Home and End jump to either end, and the selection stops there.
    key(&mut application, KeyCode::Home);
    assert!(legend_heading(&application).starts_with("Tue 29 Sep ·"));
    key(&mut application, KeyCode::Left);
    assert!(legend_heading(&application).starts_with("Tue 29 Sep ·"));
    key(&mut application, KeyCode::Right);
    assert!(legend_heading(&application).starts_with("Wed 30 Sep ·"));
    key(&mut application, KeyCode::End);
    assert!(legend_heading(&application).starts_with("Wed 28 Oct ·"));

    // → enters from the left, at the first bucket.
    let mut application = thirty_days_of_tokens();
    key(&mut application, KeyCode::Right);
    assert!(legend_heading(&application).starts_with("Tue 29 Sep ·"));

    // The commands do as their keys do.
    let mut by_key = thirty_days_of_tokens();
    let mut by_command = thirty_days_of_tokens();
    for (code, command) in [
        (KeyCode::End, SemanticCommandId::MetricsBucketLast),
        (KeyCode::Left, SemanticCommandId::MetricsBucketPrevious),
        (KeyCode::Left, SemanticCommandId::MetricsBucketPrevious),
        (KeyCode::Home, SemanticCommandId::MetricsBucketFirst),
        (KeyCode::Right, SemanticCommandId::MetricsBucketNext),
    ] {
        key(&mut by_key, code);
        invoke(&mut by_command, command);
        assert_eq!(screen(&by_key), screen(&by_command));
    }
    invoke(
        &mut by_command,
        SemanticCommandId::MetricsBucketSelect { bucket: 14 },
    );
    assert!(legend_heading(&by_command).starts_with("Tue 13 Oct ·"));
    invoke(
        &mut by_command,
        SemanticCommandId::MetricsBucketSelect { bucket: 99 },
    );
    assert!(
        legend_heading(&by_command).starts_with("Wed 28 Oct ·"),
        "a bucket past the last selects the last"
    );
}

#[test]
fn a_selected_bar_reads_its_own_figures_in_the_legend() {
    let mut application = thirty_days_of_tokens();
    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 14 },
    );

    // The fifteenth day: Opus 5M, Codex 1M, Copilot 250K, by three Sessions.
    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    assert_eq!(
        legend_heading(&application),
        "Tue 13 Oct · 6.25M tokens · 3 Sessions"
    );
    assert!(!shown.contains("Last 30 days"), "{shown}");
    for (label, figures) in [
        ("Claude Opus 5.5", ["5M ", "80%"]),
        ("GPT-5.5 Codex", ["1M ", "16%"]),
        ("copilot · GPT-5.5", ["250K", "4%"]),
    ] {
        let line = row(&rows, label);
        for figure in figures {
            assert!(line.contains(figure), "{figure} in {line:?}");
        }
    }

    // The selection outlives a change of measure, its figures in the new one.
    press(&mut application, 'c');
    assert_eq!(
        legend_heading(&application),
        "Tue 13 Oct · $0.00 · 3 Sessions"
    );

    // Sessions count the bucket's distinct total, which the Models overcount.
    press(&mut application, 's');
    let rows = screen(&application);
    assert_eq!(legend_heading(&application), "Tue 13 Oct · 3 Sessions");
    for (label, sessions) in [
        ("Claude Opus 5.5", "2 "),
        ("GPT-5.5 Codex", "1 "),
        ("copilot · GPT-5.5", "1 "),
    ] {
        let line = row(&rows, label);
        assert!(line.contains(sessions), "{sessions} in {line:?}");
    }
    row(
        &rows,
        "A Session that used several Models counts once for each",
    );

    // Clearing it restores the range's figures.
    show_tokens(&mut application);
    invoke(&mut application, SemanticCommandId::MetricsBucketClear);
    assert_eq!(legend_heading(&application), "Last 30 days · 109M tokens");
    row(&screen(&application), "90M");
}

#[test]
fn a_selected_bar_s_cost_is_partial_where_its_own_work_is() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));
    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 14 },
    );

    let rows = screen(&application);
    assert_eq!(
        legend_heading(&application),
        "Tue 13 Oct · $7.00+ · 3 Sessions"
    );
    for (label, figures) in [
        ("Claude Opus 5.5", ["$5.00 ", "71%"]),
        ("GPT-5.5 Codex", ["$2.00 ", "29%"]),
        ("copilot · GPT-5.5", ["$0.00+", "0%"]),
    ] {
        let line = row(&rows, label);
        for figure in figures {
            assert!(line.contains(figure), "{figure} in {line:?}");
        }
    }

    // The second day has no Copilot, so its Cost is all it says.
    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 1 },
    );
    assert_eq!(
        legend_heading(&application),
        "Wed 30 Sep · $3.00 · 2 Sessions"
    );
}

#[test]
fn the_selected_bar_is_marked_under_the_axis() {
    let mut application = thirty_days_of_tokens();
    let marks = |application: &Application| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        let (_, axis_y) = text_position(&buffer, "└");
        (0..WIDTH)
            .filter(|x| buffer[(*x, axis_y + 1)].symbol() == "▔")
            .collect::<Vec<_>>()
    };
    assert!(marks(&application).is_empty());

    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 14 },
    );
    let (column, _) = bar_at(&application, 14);
    assert_eq!(marks(&application), vec![column, column + 1]);
    eprintln!("{}", screen(&application).join("\n"));
}

#[test]
fn esc_clears_the_selection_and_then_closes() {
    assert_eq!(SemanticCommandId::MetricsClose.as_str(), "metrics.close");
    let mut application = thirty_days_of_tokens();
    let esc = InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        application.command_for_terminal_input(esc.clone()),
        Some(CommandId::InvokeSemantic(SemanticCommandId::MetricsClose))
    );
    key(&mut application, KeyCode::End);
    assert_eq!(
        application.command_for_terminal_input(esc),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsBucketClear
        ))
    );

    key(&mut application, KeyCode::Esc);
    assert!(
        screen(&application)[0].starts_with(" Metrics"),
        "still open"
    );
    assert!(legend_heading(&application).starts_with("Last 30 days"));
    key(&mut application, KeyCode::Esc);
    assert!(!screen(&application)[0].starts_with(" Metrics"), "closed");
}

#[test]
fn a_click_selects_a_bar_and_one_on_empty_chart_space_clears_it() {
    let mut application = thirty_days_of_tokens();
    let bar = bar_at(&application, 14);
    press_at(&mut application, bar);
    assert!(legend_heading(&application).starts_with("Tue 13 Oct ·"));

    // The gap before a bar, and the space above a short one, are still its.
    let (column, _) = bar_at(&application, 0);
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (_, top) = text_position(&buffer, "7M ┤");
    press_at(&mut application, (column - 1, top));
    assert!(legend_heading(&application).starts_with("Tue 29 Sep ·"));

    let mut by_command = thirty_days_of_tokens();
    invoke(
        &mut by_command,
        SemanticCommandId::MetricsBucketSelect { bucket: 0 },
    );
    assert_eq!(screen(&application), screen(&by_command));

    // Beside the axis, among its ticks, nothing stands to select.
    let (left, _) = top_left_of_chart(&application);
    press_at(&mut application, (left, top));
    assert!(legend_heading(&application).starts_with("Last 30 days"));

    // Wheeling over a bar still steps the range.
    let bar = bar_at(&application, 14);
    expect_read(wheel(&mut application, MouseEventKind::ScrollDown, bar));
}

/// The chart's first column and its first row.
fn top_left_of_chart(application: &Application) -> (u16, u16) {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, "7M ┤");
    (x, y)
}

#[test]
fn dragging_across_the_bars_moves_the_selection_with_the_pointer() {
    let mut application = thirty_days_of_tokens();
    let start = bar_at(&application, 3);
    let command_for = |application: &Application, kind, (column, row): (u16, u16)| {
        application.command_for_terminal_input(InputEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    };
    // The press and the drag go the way every pointer's do.
    assert_eq!(
        command_for(&application, MouseEventKind::Down(MouseButton::Left), start),
        Some(CommandId::PressAt {
            position: ratatui::layout::Position::new(start.0, start.1),
            modifiers: KeyModifiers::NONE,
        })
    );

    press_at_only(&mut application, start);
    {
        let at = bar_at(&application, 5);
        drag_to(&mut application, at);
    }
    assert!(
        legend_heading(&application).starts_with("Sun 4 Oct ·"),
        "{}",
        legend_heading(&application)
    );
    {
        let at = bar_at(&application, 14);
        drag_to(&mut application, at);
    }
    assert!(legend_heading(&application).starts_with("Tue 13 Oct ·"));

    // Past either end of the bars the selection stays at that end.
    let (left, row) = top_left_of_chart(&application);
    drag_to(&mut application, (left, row + 2));
    assert!(legend_heading(&application).starts_with("Tue 29 Sep ·"));
    drag_to(&mut application, (WIDTH - 1, row + 2));
    assert!(legend_heading(&application).starts_with("Wed 28 Oct ·"));

    // Letting go where the drag ends keeps what it chose, rather than
    // clicking there.
    let end = bar_at(&application, 20);
    drag_to(&mut application, end);
    release_at(&mut application, end);
    assert!(legend_heading(&application).starts_with("Mon 19 Oct ·"));
    let mut by_command = thirty_days_of_tokens();
    invoke(
        &mut by_command,
        SemanticCommandId::MetricsBucketSelect { bucket: 20 },
    );
    assert_eq!(screen(&application), screen(&by_command));

    // A drag that ends where it began still keeps it.
    let back = bar_at(&application, 20);
    press_at_only(&mut application, back);
    {
        let at = bar_at(&application, 21);
        drag_to(&mut application, at);
    }
    drag_to(&mut application, back);
    release_at(&mut application, back);
    assert!(legend_heading(&application).starts_with("Mon 19 Oct ·"));
}

#[test]
fn a_drag_that_returns_to_empty_space_and_lets_go_there_is_not_a_click() {
    let mut application = thirty_days_of_tokens();
    // Beside the axis, among its ticks: a click there clears the selection.
    let (left, row) = top_left_of_chart(&application);
    let beside = (left, row + 2);
    key(&mut application, KeyCode::End);
    press_at(&mut application, beside);
    assert!(legend_heading(&application).starts_with("Last 30 days"));

    // Pressed there, dragged onto the bars and back, then let go where it
    // was pressed: the drag chose, and letting go is no click.
    press_at_only(&mut application, beside);
    {
        let at = bar_at(&application, 5);
        drag_to(&mut application, at);
    }
    assert!(legend_heading(&application).starts_with("Sun 4 Oct ·"));
    drag_to(&mut application, beside);
    release_at(&mut application, beside);
    assert!(
        legend_heading(&application).starts_with("Tue 29 Sep ·"),
        "{}",
        legend_heading(&application)
    );
}

fn press_at_only(application: &mut Application, (column, row): (u16, u16)) {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("press the pointer");
}

#[test]
fn choosing_a_range_or_breakdown_clears_the_selection_and_a_refresh_keeps_it() {
    let mut application = thirty_days_of_tokens();
    key(&mut application, KeyCode::End);
    let (_, request_id, query) = expect_read(press(&mut application, 'r'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert!(legend_heading(&application).starts_with("Wed 28 Oct ·"));

    let (_, request_id, query) = expect_read(press(&mut application, '2'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert!(
        legend_heading(&application).starts_with("Last 7 days"),
        "{}",
        legend_heading(&application)
    );

    key(&mut application, KeyCode::End);
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    let mut answered = report(&query);
    answered.keys = vec![UsageMetricsKeyLabel {
        key: opus(),
        label: "Claude Opus 5.5".to_owned(),
    }];
    answer(&mut application, request_id, Ok(answered));
    assert!(legend_heading(&application).starts_with("Last 7 days"));
}

#[test]
fn with_nothing_to_draw_there_is_no_bar_to_select() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    let esc = InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    key(&mut application, KeyCode::End);
    assert_eq!(
        application.command_for_terminal_input(esc.clone()),
        Some(CommandId::InvokeSemantic(SemanticCommandId::MetricsClose)),
        "nothing read yet, so nothing selected"
    );

    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(empty_report(&query, None)));
    key(&mut application, KeyCode::End);
    assert_eq!(
        application.command_for_terminal_input(esc),
        Some(CommandId::InvokeSemantic(SemanticCommandId::MetricsClose)),
        "an empty range has no bars"
    );
}

#[test]
fn a_refresh_keeps_the_selected_day_where_the_new_range_still_holds_it() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let now = Arc::new(AtomicU64::new(afternoon().0));
    let clock = Arc::clone(&now);
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path())
        .with_session_clock(move || SessionTimestamp(clock.load(Ordering::SeqCst)))
        .with_time_zone(zone(NEW_YORK));
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    rendered_application_buffer(&application, WIDTH, HEIGHT);
    key(&mut application, KeyCode::End);
    assert!(legend_heading(&application).starts_with("Wed 28 Oct ·"));

    // A day on, the 28th is no longer the last bucket, and is still chosen.
    let a_day = 24 * 60 * 60 * 1_000;
    now.fetch_add(a_day, Ordering::SeqCst);
    let (_, request_id, query) = expect_read(press(&mut application, 'r'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert!(
        legend_heading(&application).starts_with("Wed 28 Oct ·"),
        "{}",
        legend_heading(&application)
    );

    // The range's first day, once a refresh has left it behind, is let go.
    key(&mut application, KeyCode::Home);
    assert!(legend_heading(&application).starts_with("Wed 30 Sep ·"));
    now.fetch_add(a_day, Ordering::SeqCst);
    let (_, request_id, query) = expect_read(press(&mut application, 'r'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert!(legend_heading(&application).starts_with("Last 30 days"));
}

#[test]
fn where_too_narrow_to_draw_every_bar_the_selection_stops_at_the_last_drawn() {
    const NARROW: u16 = 30;
    let mut application = thirty_days_of_tokens();
    let marks = |application: &Application| {
        let buffer = rendered_application_buffer(application, NARROW, HEIGHT);
        let (axis_x, axis_y) = text_position(&buffer, "└");
        let bars = (0..NARROW)
            .filter(|x| buffer[(*x, axis_y - 1)].symbol() != " " && *x > axis_x)
            .count();
        let marked: Vec<u16> = (0..NARROW)
            .filter(|x| buffer[(*x, axis_y + 1)].symbol() == "▔")
            .map(|x| x - axis_x - 1)
            .collect();
        (bars, marked)
    };
    // One column apiece, and room for fewer than thirty.
    let (drawn, _) = marks(&application);
    assert!(drawn < 30, "{drawn} bars");

    key(&mut application, KeyCode::End);
    let last = u16::try_from(drawn - 1).unwrap();
    assert_eq!(marks(&application).1, vec![last]);
    key(&mut application, KeyCode::Right);
    assert_eq!(marks(&application).1, vec![last]);
    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 29 },
    );
    assert_eq!(marks(&application).1, vec![last]);
    key(&mut application, KeyCode::Home);
    key(&mut application, KeyCode::Right);
    assert_eq!(marks(&application).1, vec![1]);
}

/// The ticks up the y-axis, lowest first.
fn ticks(application: &Application) -> Vec<String> {
    let mut ticks: Vec<String> = screen(application)
        .iter()
        .filter_map(|row| {
            let (label, _) = row.split_once(" ┤")?;
            Some(label.trim().to_owned())
        })
        .collect();
    ticks.reverse();
    ticks
}

#[test]
fn hiding_the_largest_series_rescales_the_axis_to_what_remains() {
    assert_eq!(
        SemanticCommandId::MetricsSeriesToggle { series: 0 }.as_str(),
        "metrics.series.toggle"
    );
    let mut application = thirty_days_of_tokens();
    assert_eq!(ticks(&application).last().map(String::as_str), Some("7M"));

    // Without Opus, the busiest day holds Codex's 1M and Copilot's 250K,
    // which the axis reaches in steps of 200K.
    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 0 },
    );

    let shown = screen(&application).join("\n");
    eprintln!("{shown}");
    assert_eq!(
        ticks(&application).last().map(String::as_str),
        Some("1.4M"),
        "{shown}"
    );
    // The legend still reads the whole range.
    row(&screen(&application), "Last 30 days · 109M tokens");
    row(&screen(&application), "Claude Opus 5.5");

    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 0 },
    );
    assert_eq!(ticks(&application).last().map(String::as_str), Some("7M"));
}

/// Whether the legend row naming `label` reads as hidden — its swatch hollow
/// and its label in the subdued colour the view's other quiet text takes —
/// or as shown, with neither; `None` where it is half of each.
fn legend_row_dimmed(application: &Application, label: &str) -> Option<bool> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, label);
    let (refresh_x, refresh_y) = text_position(&buffer, "r refresh");
    let subdued = buffer[(refresh_x, refresh_y)].fg;
    let hollow = buffer[(x - 2, y)].symbol() == "░";
    let quiet = buffer[(x, y)].fg == subdued;
    (hollow == quiet).then_some(hollow)
}

#[test]
fn a_hidden_series_legend_row_is_dimmed_and_keeps_its_figures() {
    let mut application = thirty_days_of_tokens();
    assert_eq!(
        legend_row_dimmed(&application, "GPT-5.5 Codex"),
        Some(false)
    );

    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 1 },
    );

    assert_eq!(legend_row_dimmed(&application, "GPT-5.5 Codex"), Some(true));
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(false)
    );
    let rows = screen(&application);
    let codex = row(&rows, "GPT-5.5 Codex");
    assert!(codex.contains("15M") && codex.contains("14%"), "{codex}");
}

#[test]
fn showing_one_series_alone_hides_every_other_and_again_restores_them_all() {
    assert_eq!(
        SemanticCommandId::MetricsSeriesSolo { series: 0 }.as_str(),
        "metrics.series.solo"
    );
    let mut application = thirty_days_of_tokens();
    let dimmed = |application: &Application| {
        ["Claude Opus 5.5", "GPT-5.5 Codex", "copilot · GPT-5.5"]
            .map(|label| legend_row_dimmed(application, label))
    };
    let solo_codex = |application: &mut Application| {
        invoke(
            application,
            SemanticCommandId::MetricsSeriesSolo { series: 1 },
        );
    };

    solo_codex(&mut application);
    assert_eq!(dimmed(&application), [Some(true), Some(false), Some(true)]);
    // Codex alone does 1M on its busiest days.
    assert_eq!(ticks(&application).last().map(String::as_str), Some("1M"));

    solo_codex(&mut application);
    assert_eq!(dimmed(&application), [Some(false); 3]);
    assert_eq!(ticks(&application).last().map(String::as_str), Some("7M"));

    // Shown alone after being hidden, a series is shown, and only it.
    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 1 },
    );
    solo_codex(&mut application);
    assert_eq!(dimmed(&application), [Some(true), Some(false), Some(true)]);
}

fn space(application: &mut Application, modifiers: KeyModifiers) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            modifiers,
        )))
        .expect("press Space")
}

/// The legend row the keyboard acts on, by its label.
fn focused_row(application: &Application) -> Option<String> {
    let rows = screen(application);
    let focused: Vec<&str> = rows
        .iter()
        .filter_map(|row| {
            let row = row.trim_start();
            row.strip_prefix("›█").or_else(|| row.strip_prefix("›░"))
        })
        .collect();
    assert!(focused.len() <= 1, "{rows:#?}");
    focused
        .first()
        .map(|row| row.split("  ").next().unwrap().trim().to_owned())
}

#[test]
fn up_and_down_focus_a_legend_row_that_space_hides_and_shift_space_shows_alone() {
    assert_eq!(
        [
            SemanticCommandId::MetricsFocusPrevious,
            SemanticCommandId::MetricsFocusNext,
        ]
        .map(SemanticCommandId::as_str),
        ["metrics.focus.previous", "metrics.focus.next"]
    );
    let mut application = thirty_days_of_tokens();
    assert_eq!(focused_row(&application), None);
    // With no row focused, Space has nothing to act on.
    assert_eq!(
        application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE
        ))),
        None
    );

    key(&mut application, KeyCode::Down);
    assert_eq!(
        focused_row(&application).as_deref(),
        Some("Claude Opus 5.5")
    );
    space(&mut application, KeyModifiers::NONE);
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(true)
    );
    assert_eq!(ticks(&application).last().map(String::as_str), Some("1.4M"));
    space(&mut application, KeyModifiers::NONE);
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(false)
    );

    // The focus stops at either end, and ↑ with none focused takes the last.
    for _ in 0..5 {
        key(&mut application, KeyCode::Down);
    }
    assert_eq!(
        focused_row(&application).as_deref(),
        Some("copilot · GPT-5.5")
    );
    key(&mut application, KeyCode::Up);
    assert_eq!(focused_row(&application).as_deref(), Some("GPT-5.5 Codex"));
    assert_eq!(
        application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::SHIFT
        ))),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsSeriesSolo { series: 1 }
        ))
    );
    space(&mut application, KeyModifiers::SHIFT);
    assert_eq!(ticks(&application).last().map(String::as_str), Some("1M"));
    // `o`, for "only", does the same where a terminal sends Shift+Space as
    // Space alone.
    press(&mut application, 'o');
    assert_eq!(ticks(&application).last().map(String::as_str), Some("7M"));

    let mut from_the_end = thirty_days_of_tokens();
    key(&mut from_the_end, KeyCode::Up);
    assert_eq!(
        focused_row(&from_the_end).as_deref(),
        Some("copilot · GPT-5.5")
    );
    key(&mut from_the_end, KeyCode::Up);
    key(&mut from_the_end, KeyCode::Up);
    key(&mut from_the_end, KeyCode::Up);
    assert_eq!(
        focused_row(&from_the_end).as_deref(),
        Some("Claude Opus 5.5")
    );
}

#[test]
fn esc_clears_the_selection_then_the_focus_and_then_closes() {
    assert_eq!(
        SemanticCommandId::MetricsFocusClear.as_str(),
        "metrics.focus.clear"
    );
    let mut application = thirty_days_of_tokens();
    let esc = InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    key(&mut application, KeyCode::End);
    key(&mut application, KeyCode::Down);
    space(&mut application, KeyModifiers::NONE);

    assert_eq!(
        application.command_for_terminal_input(esc.clone()),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsBucketClear
        ))
    );
    key(&mut application, KeyCode::Esc);
    assert!(legend_heading(&application).starts_with("Last 30 days"));
    assert_eq!(
        focused_row(&application).as_deref(),
        Some("Claude Opus 5.5")
    );

    assert_eq!(
        application.command_for_terminal_input(esc.clone()),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsFocusClear
        ))
    );
    key(&mut application, KeyCode::Esc);
    assert_eq!(focused_row(&application), None);
    // Letting go of the focus shows nothing hidden again.
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(true)
    );

    assert_eq!(
        application.command_for_terminal_input(esc),
        Some(CommandId::InvokeSemantic(SemanticCommandId::MetricsClose))
    );
}

fn click_with(
    application: &mut Application,
    (column, row): (u16, u16),
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers,
        },
    )
    .expect("click the pointer")
}

/// Where the legend row naming `label` stands: a column of its figures.
fn legend_row_at(application: &Application, label: &str) -> (u16, u16) {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, label);
    (x + 2, y)
}

#[test]
fn a_click_on_a_legend_row_hides_it_and_a_modified_click_shows_it_alone() {
    let mut application = thirty_days_of_tokens();
    let codex = legend_row_at(&application, "GPT-5.5 Codex");
    press_at(&mut application, codex);

    let mut by_keys = thirty_days_of_tokens();
    key(&mut by_keys, KeyCode::Down);
    key(&mut by_keys, KeyCode::Down);
    space(&mut by_keys, KeyModifiers::NONE);
    assert_eq!(screen(&application), screen(&by_keys));
    let mut by_command = thirty_days_of_tokens();
    invoke(
        &mut by_command,
        SemanticCommandId::MetricsSeriesToggle { series: 1 },
    );
    assert_eq!(screen(&application), screen(&by_command));
    assert_eq!(legend_row_dimmed(&application, "GPT-5.5 Codex"), Some(true));

    // The swatch, and the row's width beyond its figures, are the row's too.
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (swatch, y) = text_position(&buffer, "GPT-5.5 Codex");
    press_at(&mut application, (swatch - 2, y));
    assert_eq!(
        legend_row_dimmed(&application, "GPT-5.5 Codex"),
        Some(false)
    );

    // Shift is often kept by the terminal for its own selection, so Ctrl or
    // Alt held over a click shows the series alone as well.
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
    ] {
        let mut application = thirty_days_of_tokens();
        let codex = legend_row_at(&application, "GPT-5.5 Codex");
        click_with(&mut application, codex, modifiers);
        let mut by_keys = thirty_days_of_tokens();
        key(&mut by_keys, KeyCode::Down);
        key(&mut by_keys, KeyCode::Down);
        space(&mut by_keys, KeyModifiers::SHIFT);
        assert_eq!(screen(&application), screen(&by_keys), "{modifiers:?}");
        assert_eq!(ticks(&application).last().map(String::as_str), Some("1M"));
        click_with(&mut application, codex, modifiers);
        assert_eq!(ticks(&application).last().map(String::as_str), Some("7M"));
    }

    // Under the legend's last row, nothing stands to hide.
    let mut application = thirty_days_of_tokens();
    let (column, y) = legend_row_at(&application, "copilot · GPT-5.5");
    press_at(&mut application, (column, y + 1));
    assert_eq!(focused_row(&application), None);
}

/// [`report`] with Opus a hundredth as busy, so it ranks last.
fn opus_falls_behind(query: &UsageMetricsQuery) -> UsageMetricsReport {
    let mut answered = report(query);
    for bucket in &mut answered.buckets {
        for keyed in &mut bucket.by_key {
            if keyed.key == opus() {
                keyed.aggregate.fresh_input_tokens = keyed
                    .aggregate
                    .fresh_input_tokens
                    .map(|tokens| tokens / 100);
            }
        }
    }
    take_together(&mut answered);
    answered
}

#[test]
fn a_hidden_series_stays_hidden_through_a_measure_and_a_refresh_but_not_a_new_range_or_breakdown() {
    let mut application = thirty_days_of_tokens();
    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 0 },
    );

    key(&mut application, KeyCode::Tab);
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(true)
    );
    show_tokens(&mut application);

    // Refreshed, Opus ranks last, and it is Opus that stays hidden.
    let (_, request_id, query) = expect_read(press(&mut application, 'r'));
    answer(&mut application, request_id, Ok(opus_falls_behind(&query)));
    let rows = screen(&application);
    assert!(
        rendered_row(&rows, "Claude Opus 5.5") > rendered_row(&rows, "copilot · GPT-5.5"),
        "{rows:#?}"
    );
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(true)
    );
    assert_eq!(
        legend_row_dimmed(&application, "GPT-5.5 Codex"),
        Some(false)
    );
    assert_eq!(
        focused_row(&application).as_deref(),
        Some("Claude Opus 5.5")
    );

    let (_, request_id, query) = expect_read(press(&mut application, '2'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(false)
    );
    assert_eq!(focused_row(&application), None);

    invoke(
        &mut application,
        SemanticCommandId::MetricsSeriesToggle { series: 0 },
    );
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    answer(&mut application, request_id, Ok(report(&query)));
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(false)
    );
}

#[test]
fn other_is_a_series_to_hide_or_show_alone() {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    expect_read(press(&mut application, 'b'));
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    answer(&mut application, request_id, Ok(many_workspaces(&query, 9)));
    show_tokens(&mut application);

    key(&mut application, KeyCode::Up);
    assert_eq!(focused_row(&application).as_deref(), Some("Other"));
    press(&mut application, 'o');

    // Other's three do 900K a day between them.
    assert_eq!(ticks(&application).last().map(String::as_str), Some("900K"));
    assert_eq!(legend_row_dimmed(&application, "Other"), Some(false));
    for index in 0..6 {
        assert_eq!(
            legend_row_dimmed(&application, &format!("workspace-{index}")),
            Some(true)
        );
    }
    space(&mut application, KeyModifiers::NONE);
    assert_eq!(legend_row_dimmed(&application, "Other"), Some(true));
}

#[test]
fn where_too_short_to_list_every_series_the_focus_stops_at_the_last_drawn() {
    const SHORT: u16 = 9;
    let mut application = thirty_days_of_tokens();
    let rows = rendered_application_rows_at(&application, WIDTH, SHORT);
    assert!(
        rows.iter().any(|row| row.contains("Claude Opus 5.5"))
            && !rows.iter().any(|row| row.contains("GPT-5.5 Codex")),
        "{rows:#?}"
    );

    key(&mut application, KeyCode::Up);
    key(&mut application, KeyCode::Down);
    assert_eq!(
        application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE
        ))),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::MetricsSeriesToggle { series: 0 }
        ))
    );
}

#[test]
fn where_too_short_to_list_any_series_nothing_can_be_focused() {
    // Eight rows leave the legend its heading alone; six leave it nothing.
    for short in [8, 6] {
        let mut application = thirty_days_of_tokens();
        let rows = rendered_application_rows_at(&application, WIDTH, short);
        assert!(
            !rows.iter().any(|row| row.contains("Claude Opus 5.5")),
            "{rows:#?}"
        );

        key(&mut application, KeyCode::Down);
        key(&mut application, KeyCode::Up);
        assert_eq!(
            application.command_for_terminal_input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char(' '),
                KeyModifiers::NONE
            ))),
            None,
            "{short} rows"
        );
    }
}
