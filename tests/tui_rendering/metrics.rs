//! `/metrics`: the Usage Metrics of every Session on the Server the Outlook is
//! turned toward, drawn over the whole frame.

use crate::tui_rendering::support::{
    application_looking_at_studio, click_mouse, connected_application, deliver_settings,
    enter_session, grace_elapses, invoke, key, rendered_application_buffer,
    rendered_application_rows_at, studio_stops_answering, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jiff::{ToSpan, civil::date, tz::TimeZone};
use ratatui::style::Color;
use suru::{
    protocol::{
        EffectiveSettings, KeyedUsageMetrics, ModelId, Outlook, ProviderId, SessionTimestamp,
        SidebarVisibility, UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsBucket,
        UsageMetricsInterval, UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery,
        UsageMetricsReport,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
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
    result: Result<UsageMetricsReport, String>,
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
fn report(query: &UsageMetricsQuery) -> UsageMetricsReport {
    let buckets = (0..query.buckets.len() as u64)
        .map(|day| {
            let parts = [
                (opus(), (day % 5 + 1) * 1_000_000),
                (codex(), (day % 3) * 500_000),
                (copilot(), if day % 2 == 0 { 250_000 } else { 0 }),
            ];
            UsageMetricsBucket {
                total: tokens(parts.iter().map(|(_, tokens)| tokens).sum()),
                by_key: parts
                    .into_iter()
                    .map(|(key, tokens_)| KeyedUsageMetrics {
                        key,
                        aggregate: tokens(tokens_),
                    })
                    .collect(),
            }
        })
        .collect();
    UsageMetricsReport {
        buckets,
        untimed: UsageMetricsBucket::default(),
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
    }
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

    let rows = screen(&application);
    let shown = rows.join("\n");
    eprintln!("{shown}");
    assert!(rows[0].trim_end().ends_with("esc ✕"), "{shown}");
    row(&rows, "Tokens by Model · Last 30 days");

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
            earliest_turn_at: None,
            keys: Vec::new(),
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
        Err("A Usage Metrics query's buckets overlap".to_owned()),
    );

    let rows = screen(&application);
    row(&rows, "Usage Metrics could not be read");
    row(&rows, "A Usage Metrics query's buckets overlap");
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
    answer(&mut application, first, Err("too late".to_owned()));
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
