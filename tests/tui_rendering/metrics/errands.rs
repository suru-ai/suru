//! The Errands series: Suru's own work, which the Server counts under a
//! reserved key whatever the breakdown, drawn as a series of its own that
//! never folds into Other and always takes the colour reserved for it.

use super::{
    HEIGHT, NEW_YORK, WIDTH, afternoon, answer, ask, click_with, expect_read, headline,
    legend_row_at, legend_row_dimmed, many_workspaces, press, press_at, priced_report, reader_in,
    rendered_row, report, row, screen, show_tokens, take_together,
};
use crate::tui_rendering::support::{invoke, rendered_application_buffer, text_position};
use crossterm::event::KeyModifiers;
use ratatui::style::Color;
use suru::{
    protocol::{
        Cost, CountedCost, KeyedUsageMetrics, UsageMetricsAggregate, UsageMetricsKey,
        UsageMetricsKeyLabel, UsageMetricsQuery, UsageMetricsReport,
    },
    tui::{Application, SemanticCommandId},
};

/// `report` with Suru's Errands beside its keys, as the Server adds them:
/// each day `fresh` tokens of fresh input and `cached` read from cache,
/// costing `nano_usd`, in every bucket's whole and under the Errands key —
/// never as a Session or a Turn.
fn with_errands(
    mut report: UsageMetricsReport,
    fresh: u64,
    cached: u64,
    nano_usd: u64,
) -> UsageMetricsReport {
    let part = UsageMetricsAggregate {
        fresh_input_tokens: Some(fresh),
        cache_read_tokens: Some(cached),
        cost: CountedCost {
            amount: Some(Cost::from_nano_usd(nano_usd)),
            is_partial: false,
        },
        ..UsageMetricsAggregate::default()
    };
    for bucket in &mut report.buckets {
        let total = &mut bucket.total;
        total.fresh_input_tokens = Some(total.fresh_input_tokens.unwrap_or(0) + fresh);
        total.cache_read_tokens = Some(total.cache_read_tokens.unwrap_or(0) + cached);
        total.cost.add(part.cost);
        bucket.by_key.push(KeyedUsageMetrics {
            key: UsageMetricsKey::Errands,
            aggregate: part,
        });
    }
    report.keys.push(UsageMetricsKeyLabel {
        key: UsageMetricsKey::Errands,
        label: "Errands".to_owned(),
    });
    take_together(&mut report);
    report
}

/// Thirty days by Workspace of `answered`, shown under Tokens.
fn by_workspace(answered: impl FnOnce(&UsageMetricsQuery) -> UsageMetricsReport) -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    ask(&mut application);
    expect_read(press(&mut application, 'b'));
    let (_, request_id, query) = expect_read(press(&mut application, 'b'));
    answer(&mut application, request_id, Ok(answered(&query)));
    show_tokens(&mut application);
    application
}

fn swatch(application: &Application, label: &str) -> Color {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let (x, y) = text_position(&buffer, label);
    buffer[(x - 2, y)].fg
}

#[test]
fn errands_never_fold_into_other_and_keep_the_colour_reserved_for_them() {
    // Nine Workspaces of 1M down to 200K a day, and Errands smaller than
    // them all: it takes the last place kept, the sixth Workspace folding
    // in its stead.
    let smallest = by_workspace(|query| with_errands(many_workspaces(query, 9), 1_000, 0, 0));

    let rows = screen(&smallest);
    let shown = rows.join("\n");
    assert!(!shown.contains("workspace-5"), "{shown}");
    assert_eq!(
        rendered_row(&rows, "Errands"),
        rendered_row(&rows, "workspace-4") + 1,
        "Errands takes its place among the series kept"
    );
    assert_eq!(
        rendered_row(&rows, "█ Other"),
        rendered_row(&rows, "Errands") + 1
    );
    // The sixth to ninth use 500K, 400K, 300K and 200K a day: 42M over thirty.
    assert!(row(&rows, "█ Other").contains("42M"), "{shown}");

    let errands = swatch(&smallest, "Errands");
    let others: Vec<Color> = (0..5)
        .map(|index| swatch(&smallest, &format!("workspace-{index}")))
        .chain([swatch(&smallest, "Other")])
        .collect();
    assert!(!others.contains(&errands), "{errands:?} among {others:?}");

    // Errands the largest of all stands first, in the same colour, and no
    // Workspace's colour moves for it.
    let largest = by_workspace(|query| with_errands(many_workspaces(query, 9), 5_000_000, 0, 0));
    let rows = screen(&largest);
    assert!(
        rendered_row(&rows, "Errands") < rendered_row(&rows, "workspace-0"),
        "{}",
        rows.join("\n")
    );
    assert_eq!(swatch(&largest, "Errands"), errands);
    for index in 0..5 {
        let label = format!("workspace-{index}");
        assert_eq!(
            swatch(&largest, &label),
            swatch(&smallest, &label),
            "{label}"
        );
    }
}

/// Thirty days of [`report`]'s three Models, and Errands of 100K a day.
fn thirty_days_with_errands() -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(
        &mut application,
        request_id,
        Ok(with_errands(report(&query), 100_000, 0, 0)),
    );
    show_tokens(&mut application);
    application
}

#[test]
fn errands_hide_and_show_alone_like_any_series() {
    let mut application = thirty_days_with_errands();
    let errands = legend_row_at(&application, "Errands");

    press_at(&mut application, errands);
    assert_eq!(legend_row_dimmed(&application, "Errands"), Some(true));
    assert_eq!(
        legend_row_dimmed(&application, "Claude Opus 5.5"),
        Some(false)
    );
    press_at(&mut application, errands);
    assert_eq!(legend_row_dimmed(&application, "Errands"), Some(false));

    click_with(&mut application, errands, KeyModifiers::CONTROL);
    assert_eq!(legend_row_dimmed(&application, "Errands"), Some(false));
    for label in ["Claude Opus 5.5", "GPT-5.5 Codex", "copilot · GPT-5.5"] {
        assert_eq!(
            legend_row_dimmed(&application, label),
            Some(true),
            "{label}"
        );
    }
}

#[test]
fn under_the_sessions_measure_the_errands_row_reads_a_dash() {
    let mut application = thirty_days_with_errands();
    press(&mut application, 's');

    let rows = screen(&application);
    let errands = row(&rows, "Errands");
    assert!(errands.contains("—"), "{errands:?}");
    assert!(!errands.contains(" 0 "), "{errands:?}");
    assert!(
        !errands.contains('%'),
        "a figure that measured nothing is no share of the whole: {errands:?}"
    );
}

#[test]
fn a_selected_bar_s_legend_reads_that_day_s_errands() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    let mut answered = with_errands(report(&query), 100_000, 0, 0);
    // The fifteenth day's Errands used 500K rather than 100K.
    let busiest = &mut answered.buckets[14];
    let more = 400_000;
    busiest.total.fresh_input_tokens = busiest.total.fresh_input_tokens.map(|held| held + more);
    for keyed in &mut busiest.by_key {
        if keyed.key == UsageMetricsKey::Errands {
            keyed.aggregate.fresh_input_tokens = Some(500_000);
        }
    }
    take_together(&mut answered);
    answer(&mut application, request_id, Ok(answered));
    show_tokens(&mut application);
    assert!(row(&screen(&application), "Errands").contains("3.4M"));

    invoke(
        &mut application,
        SemanticCommandId::MetricsBucketSelect { bucket: 14 },
    );

    // Opus 5M, Codex 1M, Copilot 250K and the Errands' 500K, by the three
    // Sessions alone.
    let rows = screen(&application);
    row(&rows, "Tue 13 Oct · 6.75M tokens · 3 Sessions · 30 Turns");
    let errands = row(&rows, "Errands");
    assert!(
        errands.contains("500K") && errands.contains("7%"),
        "{errands:?}"
    );
}

#[test]
fn the_headline_counts_errands_in_cost_tokens_and_cache_but_not_sessions_or_turns() {
    let headline_of = |errands: bool| {
        let mut application = reader_in(NEW_YORK, afternoon());
        let (_, request_id, query) = ask(&mut application);
        let mut answered = priced_report(&query);
        for bucket in &mut answered.buckets {
            bucket.total.turns = 10;
        }
        if errands {
            // $0.10, 100K tokens of fresh input and 900K read from cache a day.
            answered = with_errands(answered, 100_000, 900_000, 100_000_000);
        }
        answered.across_buckets.total.turns = 300;
        answered.across_buckets_and_untimed = answered.across_buckets.total;
        answer(&mut application, request_id, Ok(answered));
        headline(&screen(&application))[0].clone()
    };

    let without = headline_of(false);
    let with = headline_of(true);
    // Opus's 90M cost $90 and Codex's 15M $30; Copilot's is unpriced. The
    // Errands add $3.00 and 3M tokens over thirty days, and all the cache
    // reads: 27M of 138.75M.
    assert!(
        without.contains("$120.00+") && with.contains("$123.00+"),
        "{without}\n{with}"
    );
    assert!(
        without.contains("109M tokens") && with.contains("112M tokens"),
        "{without}\n{with}"
    );
    assert!(
        without.contains("0% from cache") && with.contains("19% from cache"),
        "{without}\n{with}"
    );
    for counted in ["15 Sessions", "300 Turns"] {
        assert!(
            without.contains(counted) && with.contains(counted),
            "{with}"
        );
    }
}
