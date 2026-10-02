//! `/context`: what occupies the open Session's context, as its Provider says.

use crate::support::{
    enter_session, key, rendered_application_rows, rendered_application_rows_at,
    type_terminal_text, workspace_dir,
};
use crossterm::event::KeyCode;
use suru::{
    protocol::{
        AgentSelection, ContextBreakdown, ContextFill, ContextItem, ContextPart, ContextSource,
        ModelId, Outlook, ProviderId, SessionId, SessionReference,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, ContextBreakdownRefusal,
        SemanticCommandId,
    },
};
use uuid::Uuid;

fn enter_idle_session(
    application: &mut Application,
    workspace: &std::path::Path,
    fill: Option<ContextFill>,
) -> SessionId {
    let (session_id, mut snapshot) = enter_session(application, workspace);
    snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("default"),
        options: Vec::new(),
    });
    snapshot.session.context_fill = fill;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the idle Session");
    session_id
}

/// Types `/context`, presses Enter, and answers with the request the press made.
fn ask(application: &mut Application, session_id: SessionId) -> Uuid {
    type_terminal_text(application, "/context");
    match key(application, KeyCode::Enter) {
        ApplicationTransition::ReadContextBreakdown {
            session,
            request_id,
        } => {
            assert_eq!(session, SessionReference::new(Outlook::Local, session_id));
            request_id
        }
        other => panic!("`/context` asks the open Session's Provider, not {other:?}"),
    }
}

fn answer(
    application: &mut Application,
    request_id: Uuid,
    result: Result<ContextBreakdown, ContextBreakdownRefusal>,
) {
    application
        .handle_event(ApplicationEvent::ContextBreakdownRead { request_id, result })
        .expect("deliver the Context Breakdown");
}

fn item(label: &str, tokens: u64) -> ContextItem {
    ContextItem {
        label: label.to_owned(),
        tokens,
    }
}

fn breakdown() -> ContextBreakdown {
    ContextBreakdown {
        fill: ContextFill {
            occupied_tokens: 50_000,
            capacity_tokens: Some(200_000),
        },
        reserved_tokens: Some(30_000),
        parts: vec![
            ContextPart {
                source: ContextSource::SystemPrompt,
                tokens: 3_000,
                items: Vec::new(),
            },
            ContextPart {
                source: ContextSource::McpTools,
                tokens: 0,
                items: Vec::new(),
            },
            ContextPart {
                source: ContextSource::Skills,
                tokens: 2_000,
                items: vec![
                    item("tdd", 300),
                    item("grilling", 900),
                    item("wizard", 200),
                    item("research", 150),
                    item("prototype", 250),
                    item("dataviz", 120),
                    item("loop", 80),
                ],
            },
            ContextPart {
                source: ContextSource::Messages,
                tokens: 45_000,
                items: vec![item("Tool results", 40_000), item("User messages", 5_000)],
            },
        ],
    }
}

fn screen(application: &Application) -> String {
    rendered_application_rows_at(application, 100, 40).join("\n")
}

/// The rendered row holding `label`, so its figures can be read beside it.
fn row<'a>(screen: &'a str, label: &str) -> &'a str {
    screen
        .lines()
        .find(|line| line.contains(label))
        .unwrap_or_else(|| panic!("no row for {label:?} in\n{screen}"))
}

#[test]
fn slash_context_shows_what_fills_the_open_sessions_context() {
    assert_eq!(
        SemanticCommandId::SessionContext.as_str(),
        "session.context"
    );
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(&mut application, workspace.path(), None);

    let request_id = ask(&mut application, session_id);
    assert!(
        screen(&application).contains("Asking the Provider"),
        "{}",
        screen(&application)
    );
    answer(&mut application, request_id, Ok(breakdown()));

    let shown = screen(&application);
    assert!(
        row(&shown, " Context ").contains(" Context "),
        "the overlay is titled: {shown}"
    );
    assert!(
        shown.contains("50K of 200K tokens (25%)"),
        "the fill heads it: {shown}"
    );
    let system = row(&shown, "System prompt");
    assert!(system.contains("3K") && system.contains("2%"), "{system}");
    assert!(
        !shown.contains("MCP tools"),
        "a source holding nothing is left out: {shown}"
    );
    let messages = row(&shown, "Messages");
    assert!(
        messages.contains("45K") && messages.contains("23%"),
        "{messages}"
    );
    assert!(row(&shown, "Tool results").contains("40K"), "{shown}");
    let reserved = row(&shown, "Reserved");
    assert!(
        reserved.contains("30K") && reserved.contains("15%"),
        "{reserved}"
    );
    let free = row(&shown, "Free");
    assert!(
        free.contains("120K") && free.contains("60%"),
        "free space is what neither fills nor reserves: {free}"
    );
}

#[test]
fn a_sources_items_are_shown_largest_first_with_the_rest_summed() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(&mut application, workspace.path(), None);
    let request_id = ask(&mut application, session_id);
    answer(&mut application, request_id, Ok(breakdown()));

    let shown = screen(&application);
    let order = ["grilling", "tdd", "prototype", "wizard", "research"].map(|label| {
        shown
            .find(&format!(" {label} "))
            .unwrap_or_else(|| panic!("{label} is among the largest: {shown}"))
    });
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "largest first: {shown}"
    );
    assert!(
        !shown.contains("dataviz") && !shown.contains(" loop "),
        "{shown}"
    );
    assert!(
        row(&shown, "2 more").contains("200"),
        "the rest are summed into one row: {shown}"
    );
}

#[test]
fn a_breakdown_without_a_window_measures_against_the_sessions_own_context_fill() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(
        &mut application,
        workspace.path(),
        Some(ContextFill {
            occupied_tokens: 48_000,
            capacity_tokens: Some(100_000),
        }),
    );
    let request_id = ask(&mut application, session_id);
    let mut windowless = breakdown();
    windowless.fill.capacity_tokens = None;
    windowless.reserved_tokens = None;
    answer(&mut application, request_id, Ok(windowless));

    let shown = screen(&application);
    assert!(shown.contains("50K of 100K tokens (50%)"), "{shown}");
    assert!(row(&shown, "Free").contains("50K"), "{shown}");
    assert!(!shown.contains("Reserved"), "{shown}");
}

#[test]
fn a_provider_that_attributes_nothing_is_explained_beside_the_fill_it_does_report() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(
        &mut application,
        workspace.path(),
        Some(ContextFill {
            occupied_tokens: 12_400,
            capacity_tokens: Some(272_000),
        }),
    );
    let request_id = ask(&mut application, session_id);
    answer(
        &mut application,
        request_id,
        Err(ContextBreakdownRefusal::Unsupported),
    );

    let shown = screen(&application);
    assert!(shown.contains("12.4K of 272K tokens (5%)"), "{shown}");
    assert!(
        shown.contains("does not say what fills"),
        "the reader learns why there is no breakdown: {shown}"
    );
}

#[test]
fn a_failed_request_says_why_and_escape_closes_the_overlay() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(&mut application, workspace.path(), None);
    let request_id = ask(&mut application, session_id);
    answer(
        &mut application,
        request_id,
        Err(ContextBreakdownRefusal::Failed(
            "Claude is not running for this Session".to_owned(),
        )),
    );
    assert!(
        screen(&application).contains("Claude is not running for this Session"),
        "{}",
        screen(&application)
    );

    key(&mut application, KeyCode::Esc);
    assert!(
        !screen(&application).contains(" Context "),
        "{}",
        screen(&application)
    );
}

#[test]
fn an_answer_to_an_earlier_request_does_not_replace_the_latest_one() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(&mut application, workspace.path(), None);
    let earlier = ask(&mut application, session_id);
    key(&mut application, KeyCode::Esc);
    let latest = ask(&mut application, session_id);

    answer(
        &mut application,
        earlier,
        Err(ContextBreakdownRefusal::Failed("stale".to_owned())),
    );
    assert!(!screen(&application).contains("stale"));
    answer(&mut application, latest, Ok(breakdown()));
    assert!(screen(&application).contains("System prompt"));
}

#[test]
fn a_long_breakdown_scrolls_within_the_overlay() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let session_id = enter_idle_session(&mut application, workspace.path(), None);
    let request_id = ask(&mut application, session_id);
    answer(&mut application, request_id, Ok(breakdown()));

    let top = rendered_application_rows_at(&application, 100, 14).join("\n");
    assert!(top.contains("System prompt"), "{top}");
    assert!(!top.contains("Free"), "{top}");
    for _ in 0..20 {
        key(&mut application, KeyCode::Down);
    }
    let bottom = rendered_application_rows_at(&application, 100, 14).join("\n");
    assert!(bottom.contains("Free"), "{bottom}");
    assert!(!bottom.contains("System prompt"), "{bottom}");
}

#[test]
fn the_landing_has_no_session_to_ask() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());

    type_terminal_text(&mut application, "/context");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    let shown = rendered_application_rows(&application).join("\n");
    assert!(
        shown.contains("Open a Session to see what fills its context"),
        "{shown}"
    );
}
