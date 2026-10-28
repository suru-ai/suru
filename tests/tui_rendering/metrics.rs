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
    let controls = row(&rows, "by Model");
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
    let controls = rendered_row(&rows, "by Model");
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
        earliest_turn_at: earliest,
        keys: Vec::new(),
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
        first.buckets.is_empty(),
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
        (query.buckets, row(&rows, "by Model").to_owned())
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
    let controls = row(&rows, "by Model");
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
            SemanticCommandId::MetricsMeasureNext,
        ]
        .map(SemanticCommandId::as_str),
        [
            "metrics.measure.cost",
            "metrics.measure.tokens",
            "metrics.measure.next",
        ]
    );
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(priced_report(&query)));
    let shows = |application: &Application, chosen: &str, total: &str| {
        let rows = screen(application);
        let controls = row(&rows, "by Model");
        assert!(controls.contains(chosen), "{chosen} in {controls:?}");
        row(&rows, total);
    };
    let tokens = ("▐Tokens▌", "Last 30 days · 109M tokens");
    let cost = ("▐Cost▌", "Last 30 days · $120.00+");
    shows(&application, cost.0, cost.1);
    let label_at = |application: &Application, label: &str| {
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        let (_, controls_y) = text_position(&buffer, "by Model");
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
            Box::new(|application: &mut Application| key(application, KeyCode::Tab)),
            tokens,
        ),
        (
            Box::new(|application: &mut Application| key(application, KeyCode::Tab)),
            cost,
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
        command_for(key_event(KeyCode::Tab)),
        semantic_command(SemanticCommandId::MetricsMeasureNext)
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
