//! An Intervention — a pending Approval or Questionnaire — presents its own
//! panel in the open Session, so the reader never has to know a key to answer.

use crate::support::{
    add_activity, add_request, approval_activity, connected_application, enter_active_session,
    invoke, key, rendered_application_rows, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ApprovalId, ApprovalOutcome, ApprovalSubject, Decision, QuestionnaireId,
        SessionSnapshot, TurnId,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

const ARMING: Duration = Duration::from_millis(250);

/// A clock the test moves by hand, so the arming window passes without any
/// test waiting out a production-scale delay.
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn advance(&self, by: Duration) {
        self.0.fetch_add(
            u64::try_from(by.as_millis()).expect("the advance fits a millisecond counter"),
            Ordering::Relaxed,
        );
    }
}

fn armed_application(workspace: &std::path::Path) -> (Application, Clock) {
    let elapsed = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&elapsed);
    let origin = Instant::now();
    let application = connected_application(workspace)
        .with_presentation_clock(move || {
            origin + Duration::from_millis(observed.load(Ordering::Relaxed))
        })
        .with_intervention_arming_delay(ARMING);
    (application, Clock(elapsed))
}

/// Records one pending Approval and answers with the id that must be decided.
fn add_pending_approval(snapshot: &mut SessionSnapshot, turn_id: TurnId, host: &str) -> ApprovalId {
    let activity = approval_activity(
        turn_id,
        ApprovalSubject::Network {
            host_or_url: host.to_owned(),
        },
        None,
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval { approval, .. } = &activity else {
        unreachable!("an Approval Activity carries an Approval")
    };
    let id = approval.id;
    snapshot.pending_approvals.push(id);
    snapshot.pending_approvals_revision = snapshot.revision;
    add_activity(snapshot, activity);
    id
}

fn deliver(app: &mut Application, snapshot: &SessionSnapshot) {
    app.handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
        snapshot.clone(),
    )))
    .expect("deliver a Session snapshot");
}

fn screen(app: &Application) -> String {
    rendered_application_rows(app).join("\n")
}

#[test]
fn pending_approval_presents_itself_with_nothing_chosen_and_takes_no_key_while_arming() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let approval_id = add_pending_approval(&mut snapshot, turn_id, "https://api.example.test/v1");
    deliver(&mut app, &snapshot);

    let presented = screen(&app);
    assert!(
        presented.contains("Approval · Choose Decision"),
        "the Approval presents itself without a key: {presented}"
    );
    assert!(
        !presented.contains("> 1. Accept once"),
        "a self-presented Approval opens with no Decision chosen: {presented}"
    );

    clock.advance(ARMING / 2);
    assert_eq!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert_eq!(
        key(&mut app, KeyCode::Down),
        ApplicationTransition::Continue
    );
    let arming = screen(&app);
    assert!(
        !arming.contains("> 1. Accept once") && !arming.contains("> 2. Accept for Session"),
        "keys inside the arming window are ignored: {arming}"
    );

    clock.advance(ARMING);
    key(&mut app, KeyCode::Down);
    let chosen = screen(&app);
    assert!(
        chosen.contains("> 1. Accept once"),
        "the first Decision is chosen once the panel is armed: {chosen}"
    );
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::SubmitDecision { id, decision: Decision::Accept, .. }
            if id == approval_id
    ));
}

#[test]
fn pending_questionnaire_presents_itself_and_swallows_printable_keys_while_arming() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    add_request(&mut snapshot, turn_id, "Choose the execution target");
    deliver(&mut app, &snapshot);

    let presented = screen(&app);
    assert!(
        presented.contains("Choose the execution target"),
        "the Questionnaire presents itself without a key: {presented}"
    );

    clock.advance(ARMING / 2);
    type_terminal_text(&mut app, "oops");
    assert!(
        !screen(&app).contains("oops"),
        "a printable key inside the arming window reaches nothing: {}",
        screen(&app)
    );

    clock.advance(ARMING);
    type_terminal_text(&mut app, "staging");
    assert!(
        screen(&app).contains("staging"),
        "the Answer takes keys once the panel is armed: {}",
        screen(&app)
    );
}

#[test]
fn the_older_intervention_presents_first_and_the_next_follows_it() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    add_pending_approval(&mut snapshot, turn_id, "https://api.example.test/v1");
    add_request(&mut snapshot, turn_id, "Choose the execution target");
    deliver(&mut app, &snapshot);

    assert!(
        screen(&app).contains("Approval · Choose Decision"),
        "the older Intervention presents first: {}",
        screen(&app)
    );

    clock.advance(ARMING * 2);
    key(&mut app, KeyCode::Char('1'));
    // The Decision settles the Approval, which the next snapshot reports.
    snapshot.revision.0 += 1;
    snapshot.pending_approvals.clear();
    snapshot.pending_approvals_revision = snapshot.revision;
    let decided = snapshot
        .activities
        .iter()
        .position(|activity| matches!(activity, Activity::Approval { .. }))
        .expect("the Approval activity is recorded");
    if let Activity::Approval {
        outcome, decision, ..
    } = &mut snapshot.activities[decided]
    {
        *outcome = ApprovalOutcome::Decided;
        *decision = Some(Decision::Accept);
    }
    deliver(&mut app, &snapshot);

    let chained = screen(&app);
    assert!(
        chained.contains("Choose the execution target"),
        "the next Intervention presents once the first settles: {chained}"
    );
    type_terminal_text(&mut app, "guarded");
    assert!(
        !screen(&app).contains("guarded"),
        "the chained presentation is guarded again: {}",
        screen(&app)
    );
}

#[test]
fn an_overlay_holds_the_presentation_until_it_closes() {
    let workspace = workspace_dir();
    let (mut app, _clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    invoke(&mut app, SemanticCommandId::ThemeList);
    add_pending_approval(&mut snapshot, turn_id, "https://api.example.test/v1");
    deliver(&mut app, &snapshot);

    assert!(
        !screen(&app).contains("Approval · Choose Decision"),
        "an overlay keeps the keys: {}",
        screen(&app)
    );

    app.handle_event(ApplicationEvent::Command(CommandId::CloseThemePicker))
        .expect("close the Theme picker");
    assert!(
        screen(&app).contains("Approval · Choose Decision"),
        "the Approval presents once the overlay closes: {}",
        screen(&app)
    );
}

#[test]
fn escape_dismisses_every_pending_intervention_until_a_new_one_arrives() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "kept draft");
    let first_id = add_pending_approval(&mut snapshot, turn_id, "https://one.example.test");
    add_request(&mut snapshot, turn_id, "Choose the execution target");
    deliver(&mut app, &snapshot);

    clock.advance(ARMING * 2);
    key(&mut app, KeyCode::Esc);
    let dismissed = screen(&app);
    assert!(
        !dismissed.contains("Approval · Choose Decision")
            && !dismissed.contains("Choose the execution target"),
        "Esc dismisses everything pending at that moment: {dismissed}"
    );
    assert!(
        dismissed.contains("kept draft"),
        "the composer comes back: {dismissed}"
    );
    assert!(
        dismissed.contains("Ctrl+Y"),
        "the Intervention notice still says how to return: {dismissed}"
    );

    // A dismissed Intervention is reopened by asking for it, with today's
    // default selection and no arming delay.
    invoke(&mut app, SemanticCommandId::ApprovalOpen);
    let reopened = screen(&app);
    assert!(
        reopened.contains("> 1. Accept once"),
        "an explicit open keeps the default selection: {reopened}"
    );
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::SubmitDecision { id, decision: Decision::Accept, .. }
            if id == first_id
    ));

    // The Decision is in flight, so Esc leaves it alone; the Server settling
    // it is what closes the panel, and the Questionnaire the explicit open
    // recalled then presents in its turn.
    key(&mut app, KeyCode::Esc);
    assert!(
        screen(&app).contains("Submitting"),
        "Esc leaves a Decision in flight standing: {}",
        screen(&app)
    );
    snapshot.revision.0 += 1;
    snapshot.pending_approvals.clear();
    snapshot.pending_approvals_revision = snapshot.revision;
    for activity in &mut snapshot.activities {
        if let Activity::Approval {
            approval,
            outcome,
            decision,
            ..
        } = activity
            && approval.id == first_id
        {
            *outcome = ApprovalOutcome::Decided;
            *decision = Some(Decision::Accept);
        }
    }
    deliver(&mut app, &snapshot);
    key(&mut app, KeyCode::Esc);
    assert!(
        !screen(&app).contains("Approval · Choose Decision"),
        "nothing is left standing once everything is dismissed again: {}",
        screen(&app)
    );

    // A later arrival is a new Intervention, so it presents itself.
    snapshot.revision.0 += 1;
    add_pending_approval(&mut snapshot, turn_id, "https://two.example.test");
    deliver(&mut app, &snapshot);
    assert!(
        screen(&app).contains("two.example.test"),
        "a new arrival presents itself: {}",
        screen(&app)
    );
}

#[test]
fn a_subagents_intervention_marks_the_parent_and_presents_in_its_own_session() {
    use suru::protocol::{SessionId, SubagentInterventions};

    let workspace = workspace_dir();
    let (mut app, _clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "kept draft");
    let child_id = SessionId::new();
    snapshot.subagent_interventions = vec![SubagentInterventions {
        session_id: child_id,
        via_session_id: child_id,
        revision: snapshot.revision,
        pending_questionnaires: vec![QuestionnaireId::new()],
        submitting_questionnaires: Vec::new(),
        pending_approvals: vec![ApprovalId::new()],
        submitting_approvals: Vec::new(),
    }];
    deliver(&mut app, &snapshot);

    let rendered = screen(&app);
    assert!(
        !rendered.contains("Approval · Choose Decision"),
        "a Subagent's Intervention only marks the listing: {rendered}"
    );
    assert!(
        rendered.contains("kept draft"),
        "the reader keeps the composer: {rendered}"
    );

    // The Decision belongs in the Session that owes it, so the Subagent's own
    // Approval presents itself once the reader is reading that Session.
    let mut child = snapshot.clone();
    child.session.id = child_id;
    child.session.parent = Some(snapshot.session.id);
    child.subagent_interventions.clear();
    add_pending_approval(&mut child, turn_id, "https://child.example.test");
    app.handle_event(ApplicationEvent::SessionAttached(child))
        .expect("open the Subagent's Session");

    let inside = screen(&app);
    assert!(
        inside.contains("Approval · Choose Decision") && inside.contains("child.example.test"),
        "the Subagent's own Intervention presents in its own Session: {inside}"
    );
}

#[test]
fn arming_drops_only_the_keys_the_panel_itself_would_take() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    add_pending_approval(&mut snapshot, turn_id, "https://api.example.test/v1");
    deliver(&mut app, &snapshot);
    clock.advance(ARMING / 2);

    assert_eq!(
        app.command_for_terminal_input(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE
        ))),
        None,
        "the panel's own keys are dropped while it arms"
    );
    assert_eq!(
        app.command_for_terminal_input(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE
        ))),
        Some(CommandId::ScrollTranscriptPageUp),
        "reading the Transcript is not the panel's key to take"
    );
    assert_eq!(
        app.command_for_terminal_input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })),
        Some(CommandId::ScrollTranscriptLinesUp),
        "the wheel goes on reading the Transcript while the panel arms"
    );

    // Esc means the same inside the window as outside it.
    key(&mut app, KeyCode::Esc);
    let dismissed = screen(&app);
    assert!(
        !dismissed.contains("Approval · Choose Decision") && dismissed.contains("Ctrl+Y"),
        "Esc dismisses from inside the arming window: {dismissed}"
    );
}

#[test]
fn escape_leaves_a_decision_in_flight_alone_until_the_server_settles_it() {
    let workspace = workspace_dir();
    let (mut app, clock) = armed_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let approval_id = add_pending_approval(&mut snapshot, turn_id, "https://api.example.test/v1");
    deliver(&mut app, &snapshot);
    clock.advance(ARMING * 2);
    assert!(matches!(
        key(&mut app, KeyCode::Char('1')),
        ApplicationTransition::SubmitDecision { id, .. } if id == approval_id
    ));
    let submitting = screen(&app);
    assert!(
        submitting.contains("Submitting"),
        "the Decision is in flight: {submitting}"
    );

    // Esc changes nothing at all: the panel stays, nothing is dismissed, and
    // no second Decision can be sent.
    key(&mut app, KeyCode::Esc);
    assert_eq!(
        screen(&app),
        submitting,
        "Esc leaves a Decision in flight exactly as it was"
    );
    assert_eq!(
        key(&mut app, KeyCode::Char('3')),
        ApplicationTransition::Continue,
        "a second Decision is never delivered"
    );
    assert_eq!(screen(&app), submitting);

    // The Server settling it is what closes the panel, as it always was.
    snapshot.revision.0 += 1;
    snapshot.pending_approvals.clear();
    snapshot.pending_approvals_revision = snapshot.revision;
    for activity in &mut snapshot.activities {
        if let Activity::Approval {
            approval,
            outcome,
            decision,
            ..
        } = activity
            && approval.id == approval_id
        {
            *outcome = ApprovalOutcome::Decided;
            *decision = Some(Decision::Accept);
        }
    }
    deliver(&mut app, &snapshot);
    assert!(
        !screen(&app).contains("Approval · Choose Decision"),
        "the settled Approval reconciles the panel closed: {}",
        screen(&app)
    );
}
