//! The Workspaces panel of the Insights tab: the range's Workspaces ranked
//! by Cost, each of which opens its trend on Overview.

use super::{answer_insights, ctrl, expect_insights_reads, panel_lines, worked};
use crate::tui_rendering::{
    metrics::{
        HEIGHT, NEW_YORK, WIDTH, afternoon, answer, ask, expect_read, legend_row_dimmed, press_at,
        reader_in, report, row, screen,
    },
    support::{invoke, key, rendered_application_buffer, text_position},
};
use crossterm::event::KeyCode;
use suru::{
    protocol::{
        Cost, KeyedUsageMetrics, UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsBucket,
        UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery, UsageMetricsReport, WorkspaceId,
    },
    tui::{
        Application, ApplicationTransition, InsightsPanel, SemanticCommandId,
        UsageMetricsReadRefusal,
    },
};

#[test]
fn switching_to_insights_reads_the_range_s_workspaces_as_one_bucket_beside_the_insights() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));

    let reads = expect_insights_reads(ctrl(&mut application, KeyCode::Right));

    assert_eq!(
        reads.workspaces,
        UsageMetricsQuery {
            buckets: vec![reads.insights.interval],
            breakdown: UsageMetricsBreakdown::Workspace,
            comparison: None,
            top_sessions: 0,
        },
        "one bucket over the interval the insights read covers, by Workspace"
    );
    assert_ne!(reads.workspaces_request_id, reads.request_id);
}

/// The Workspace key a label names, by an identity of its own.
fn workspace(label: &str) -> UsageMetricsKey {
    UsageMetricsKey::Workspace {
        workspace: WorkspaceId(format!("directory:{label}")),
    }
}

/// The one-bucket Workspace report `query` asks for, each Workspace of
/// `spent` costing its dollars, partial where it says, and listed by name
/// rather than by what it cost.
pub(super) fn spent(query: &UsageMetricsQuery, spent: &[(&str, f64, bool)]) -> UsageMetricsReport {
    let mut spent = spent.to_vec();
    spent.sort_by_key(|(label, _, _)| *label);
    let by_key: Vec<KeyedUsageMetrics> = spent
        .iter()
        .map(|(label, usd, partial)| KeyedUsageMetrics {
            key: workspace(label),
            aggregate: UsageMetricsAggregate {
                cost: Cost::from_usd(*usd),
                is_cost_partial: *partial,
                sessions: 1,
                turns: 1,
                ..UsageMetricsAggregate::default()
            },
        })
        .collect();
    let total = UsageMetricsAggregate {
        cost: Cost::from_usd(spent.iter().map(|(_, usd, _)| usd).sum()),
        is_cost_partial: spent.iter().any(|(_, _, partial)| *partial),
        sessions: spent.len() as u64,
        turns: spent.len() as u64,
        ..UsageMetricsAggregate::default()
    };
    let bucket = UsageMetricsBucket { total, by_key };
    UsageMetricsReport {
        buckets: query.buckets.iter().map(|_| bucket.clone()).collect(),
        across_buckets: bucket.clone(),
        untimed: UsageMetricsBucket::default(),
        across_buckets_and_untimed: total,
        comparison: None,
        earliest_turn_at: query.buckets.first().map(|bucket| bucket.start),
        keys: spent
            .iter()
            .map(|(label, _, _)| UsageMetricsKeyLabel {
                key: workspace(label),
                label: (*label).to_owned(),
            })
            .collect(),
        top_sessions: Vec::new(),
    }
}

/// The mockup's eight Workspaces, $413.86 between them: four ranked by
/// name, dotfiles' Cost partial, and four more costing $33.59 together.
pub(super) const MOCKUP: [(&str, f64, bool); 8] = [
    ("blog", 30.02, false),
    ("dotfiles", 41.75, true),
    ("garden", 15.0, false),
    ("notes", 10.0, false),
    ("recipes", 5.59, false),
    ("relay-infra", 96.10, false),
    ("suru", 211.40, false),
    ("zines", 3.0, false),
];

/// The view opened on 30 days and switched to Insights, both its reads
/// answered: the insights with the mockup's, and the Workspaces by what
/// `workspaces` makes of the query.
fn on_workspaces(
    workspaces: impl FnOnce(&UsageMetricsQuery) -> Result<UsageMetricsReport, UsageMetricsReadRefusal>,
) -> Application {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let reads = expect_insights_reads(ctrl(&mut application, KeyCode::Right));
    answer_insights(&mut application, reads.request_id, Ok(worked()));
    let answered = workspaces(&reads.workspaces);
    answer(&mut application, reads.workspaces_request_id, answered);
    application
}

/// The Workspaces panel's lines, each with its bar taken off and its runs
/// of spaces closed up, and how many cells each bar fills.
fn workspaces_panel(application: &Application) -> (Vec<String>, Vec<usize>) {
    let bar = |glyph: char| "█▏▎▍▌▋▊▉".contains(glyph);
    panel_lines(application, "Workspaces by Cost")
        .into_iter()
        .map(|line| {
            let cells = line.chars().filter(|glyph| bar(*glyph)).count();
            let words = line.replace(bar, " ");
            (
                words.split_whitespace().collect::<Vec<_>>().join(" "),
                cells,
            )
        })
        .unzip()
}

#[test]
fn workspaces_are_ranked_by_cost_with_a_plus_on_partial_figures_and_the_rest_folded() {
    let application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));

    let (lines, bars) = workspaces_panel(&application);

    assert_eq!(
        lines,
        vec![
            "suru $211.40",
            "relay-infra $96.10",
            "dotfiles $41.75+",
            "blog $30.02",
            "(4 more) $33.59",
        ]
    );
    assert!(
        bars[..4].windows(2).all(|pair| pair[0] >= pair[1]) && bars[3] > 0,
        "each bar as long as its Cost beside the largest: {bars:?}"
    );
    assert!(bars[0] >= 2 * bars[1], "{bars:?}");
    assert!(
        bars[4] >= bars[3],
        "the fold's bar stands for the four it folds together: {bars:?}"
    );
}

#[test]
fn a_range_with_no_work_in_any_workspace_says_so_in_the_panel() {
    let application = on_workspaces(|query| Ok(spent(query, &[])));

    assert_eq!(
        workspaces_panel(&application).0[0],
        "No work in the last 30 days"
    );
}

#[test]
fn the_panel_says_while_its_workspaces_are_read_and_why_they_could_not_be() {
    let mut application = reader_in(NEW_YORK, afternoon());
    let (_, request_id, query) = ask(&mut application);
    answer(&mut application, request_id, Ok(report(&query)));
    let reads = expect_insights_reads(ctrl(&mut application, KeyCode::Right));
    answer_insights(&mut application, reads.request_id, Ok(worked()));

    assert_eq!(workspaces_panel(&application).0[0], "Reading Workspaces…");

    answer(
        &mut application,
        reads.workspaces_request_id,
        Err(UsageMetricsReadRefusal::Refused(
            "metrics are switched off".to_owned(),
        )),
    );
    assert_eq!(
        workspaces_panel(&application).0[..2],
        [
            "The Server refused the Workspace read",
            "metrics are switched off"
        ]
    );
    row(&screen(&application), "median 48s");
}

/// Whether each of `labels`' legend rows on Overview is dimmed, its series
/// hidden.
fn dimmed<const N: usize>(application: &Application, labels: [&str; N]) -> [Option<bool>; N] {
    labels.map(|label| legend_row_dimmed(application, label))
}

/// Overview's read of the Workspaces an opened row brings, answered with
/// the mockup's Workspaces in each of its buckets.
fn answer_overview_by_workspace(application: &mut Application, transition: ApplicationTransition) {
    let (_, request_id, query) = expect_read(transition);
    assert_eq!(query.breakdown, UsageMetricsBreakdown::Workspace);
    answer(application, request_id, Ok(spent(&query, &MOCKUP)));
}

#[test]
fn enter_on_a_workspace_opens_overview_by_workspace_showing_only_it() {
    assert_eq!(
        SemanticCommandId::MetricsPanelItemOpen {
            panel: InsightsPanel::Workspaces,
            item: 0,
        }
        .as_str(),
        "metrics.panel.item.open"
    );
    let mut application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));
    invoke(
        &mut application,
        SemanticCommandId::MetricsPanelFocus(InsightsPanel::Workspaces),
    );
    key(&mut application, KeyCode::Down);
    key(&mut application, KeyCode::Down);

    let opened = key(&mut application, KeyCode::Enter);

    let rows = screen(&application);
    assert!(row(&rows, "Overview").contains("▐Overview▌"), "{rows:#?}");
    answer_overview_by_workspace(&mut application, opened);
    assert_eq!(
        dimmed(&application, ["suru", "relay-infra", "dotfiles", "blog"]),
        [Some(true), Some(false), Some(true), Some(true)],
        "relay-infra, the second row, shown alone"
    );
}

#[test]
fn a_click_on_a_workspace_opens_it_alone_on_overview_too() {
    let mut application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let blog = text_position(&buffer, "blog");

    let opened = press_at(&mut application, blog);

    answer_overview_by_workspace(&mut application, opened);
    assert_eq!(
        dimmed(&application, ["suru", "relay-infra", "dotfiles", "blog"]),
        [Some(true), Some(true), Some(true), Some(false)]
    );
}

#[test]
fn a_workspace_overview_would_fold_into_other_is_kept_apart_to_be_shown_alone() {
    let mut application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));
    // Overview keeps six Workspaces apart and folds recipes and zines,
    // the smallest, into Other.
    let opened = invoke(
        &mut application,
        SemanticCommandId::MetricsPanelItemOpen {
            panel: InsightsPanel::Workspaces,
            item: 7,
        },
    );

    answer_overview_by_workspace(&mut application, opened);
    assert_eq!(
        dimmed(&application, ["zines", "suru", "Other"]),
        [Some(false), Some(true), Some(true)],
        "zines in a row of its own, shown alone"
    );
    assert!(
        !screen(&application).iter().any(|row| row.contains("notes")),
        "notes, the smallest Workspace kept, folded into Other in its stead"
    );
}

#[test]
fn opening_a_workspace_while_overview_shows_workspaces_already_reads_nothing() {
    let mut application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));
    assert_eq!(
        key(&mut application, KeyCode::Char('g')),
        ApplicationTransition::Continue
    );
    let (_, request_id, query) = expect_read(key(&mut application, KeyCode::Char('b')));
    answer(&mut application, request_id, Ok(report(&query)));
    let (_, request_id, query) = expect_read(key(&mut application, KeyCode::Char('b')));
    answer(&mut application, request_id, Ok(spent(&query, &MOCKUP)));
    key(&mut application, KeyCode::Char('i'));

    let opened = invoke(
        &mut application,
        SemanticCommandId::MetricsPanelItemOpen {
            panel: InsightsPanel::Workspaces,
            item: 2,
        },
    );

    assert_eq!(opened, ApplicationTransition::Continue);
    assert_eq!(
        dimmed(&application, ["suru", "relay-infra", "dotfiles"]),
        [Some(true), Some(true), Some(false)]
    );
}

#[test]
fn a_workspace_opened_into_a_report_without_it_is_not_shown_alone_by_a_later_one() {
    let mut application = on_workspaces(|query| Ok(spent(query, &MOCKUP)));
    let opened = invoke(
        &mut application,
        SemanticCommandId::MetricsPanelItemOpen {
            panel: InsightsPanel::Workspaces,
            item: 1,
        },
    );
    let (_, request_id, query) = expect_read(opened);
    answer(&mut application, request_id, Ok(spent(&query, &[])));

    let (_, request_id, query) = expect_read(key(&mut application, KeyCode::Char('r')));
    answer(&mut application, request_id, Ok(spent(&query, &MOCKUP)));

    assert_eq!(
        dimmed(&application, ["suru", "relay-infra", "dotfiles"]),
        [Some(false); 3],
        "the report the Workspace was opened into had none of it to show alone"
    );
}
