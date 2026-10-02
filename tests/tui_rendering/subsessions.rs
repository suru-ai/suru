//! A Subsession in its Sidekick's Transcript: beginning one stands as a row
//! of its own kind, naming the Subsession by its Title and saying what it was
//! first asked, and the whole row is the way into it — a Session heading a
//! tree of its own, opened as a Sidekick's Session is opened from a Prompt it
//! sent. One begun on a Remote names that Remote and is opened there, the
//! Outlook turning toward it. The row is the user's way back to work the
//! Sidekick began, so a settled Turn's fold never hides it.

use crate::support::{
    click_mouse, connected_application, navigable_session_snapshot, rendered_application_buffer,
    rendered_application_rows_at, text_position, workspace_dir,
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        Activity, ActivityId, Outlook, SessionDeleted, SessionId, SessionReference,
        SessionSnapshot, TranscriptItem,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

/// A Sidekick's Session whose one settled Turn began `subsession`, titled
/// `title` and first asked `prompt`, after looking for where to begin it.
fn began_a_subsession(
    workspace: &std::path::Path,
    subsession: SessionId,
    title: &str,
    prompt: &str,
) -> SessionSnapshot {
    let mut snapshot = navigable_session_snapshot(SessionId::new(), workspace, 1);
    let turn_id = snapshot.turns[0].id;
    let looking = ActivityId::new();
    let row = ActivityId::new();
    snapshot.activities.extend([
        Activity::Status {
            id: looking,
            turn_id,
            text: "Looking for the auth suite".to_owned(),
        },
        Activity::Subsession {
            id: row,
            turn_id,
            session_id: subsession,
            origin: None,
            title: title.to_owned(),
            prompt: prompt.to_owned(),
        },
    ]);
    // Between the user's Prompt and the Sidekick's answer, as it worked.
    snapshot.transcript.splice(
        1..1,
        [
            TranscriptItem::Activity {
                activity_id: looking,
            },
            TranscriptItem::Activity { activity_id: row },
        ],
    );
    snapshot
}

/// Presses the primary pointer button on the first rendered occurrence of
/// `needle`, the way a reader clicks what they can see.
fn press_text(application: &mut Application, needle: &str) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, 120, 24);
    let (column, row) = text_position(&buffer, needle);
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press what is drawn")
}

fn attached(snapshot: SessionSnapshot, workspace: &std::path::Path) -> Application {
    let mut application = connected_application(workspace);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Sidekick's Session");
    application
}

#[test]
fn opening_a_subsession_is_a_semantic_command_its_row_invokes() {
    assert_eq!(
        SemanticCommandId::SubsessionOpen.as_str(),
        "subsession.open"
    );
}

#[test]
fn a_subsession_the_sidekick_began_is_a_row_naming_it_and_what_it_was_asked() {
    let workspace = workspace_dir();
    let subsession = SessionId::new();
    let application = attached(
        began_a_subsession(
            workspace.path(),
            subsession,
            "Flaky login test",
            "Make it pass.\nIt fails one run in ten.",
        ),
        workspace.path(),
    );
    let text = rendered_application_rows_at(&application, 120, 24).join("\n");

    assert!(
        text.contains("  ↗ Subsession: Flaky login test: Make it pass. It fails one run in ten."),
        "the row names the Subsession and says, on its one line, what it was first asked: {text}"
    );
    assert!(
        !text.contains("Tool Call") && !text.contains("begin_session"),
        "it stands as a row of its own kind, never as a Tool Call: {text}"
    );
    assert!(
        !text.contains("Looking for the auth suite"),
        "the settled Turn's fold hides the work the Sidekick did: {text}"
    );
}

#[test]
fn a_subsession_still_titled_by_its_prompt_says_it_once() {
    let workspace = workspace_dir();
    let application = attached(
        began_a_subsession(
            workspace.path(),
            SessionId::new(),
            "Fix the flaky login test",
            "Fix the flaky login test",
        ),
        workspace.path(),
    );
    let text = rendered_application_rows_at(&application, 120, 24).join("\n");

    assert!(
        text.lines()
            .any(|row| row.trim() == "↗ Subsession: Fix the flaky login test"),
        "a Title not yet derived is the Prompt itself, so the row says it once: {text}"
    );
}

#[test]
fn pressing_a_subsessions_row_opens_the_subsession() {
    let workspace = workspace_dir();
    let subsession = SessionId::new();
    let mut application = attached(
        began_a_subsession(
            workspace.path(),
            subsession,
            "Fix the flaky login test",
            "Fix the flaky login test in the auth suite.",
        ),
        workspace.path(),
    );

    assert_eq!(
        press_text(&mut application, "Subsession: Fix"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            subsession,
        )),
        "the whole row leads into the Subsession, a Session of its own"
    );
}

#[test]
fn a_subsession_begun_on_a_remote_names_its_remote_and_opens_there() {
    let workspace = workspace_dir();
    let subsession = SessionId::new();
    let mut snapshot = began_a_subsession(
        workspace.path(),
        subsession,
        "Fix the flaky login test",
        "Fix the flaky login test in the auth suite.",
    );
    for activity in &mut snapshot.activities {
        if let Activity::Subsession { origin, .. } = activity {
            *origin = Some("workstation".to_owned());
        }
    }
    let mut application = attached(snapshot, workspace.path());
    let text = rendered_application_rows_at(&application, 120, 24).join("\n");

    assert!(
        text.contains("↗ Subsession on workstation: Fix the flaky login test: Fix the flaky"),
        "the row names the Remote it was begun on: {text}"
    );
    assert!(
        matches!(
            press_text(&mut application, "Subsession on workstation"),
            ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. }
                if session == SessionReference::new(
                    Outlook::Remote("workstation".to_owned()),
                    subsession,
                )
        ),
        "the whole row leads into it on that Remote, turning the Outlook there"
    );
}

#[test]
fn a_subsession_that_is_gone_is_still_named_but_offers_no_way_in() {
    let workspace = workspace_dir();
    let subsession = SessionId::new();
    let mut application = attached(
        began_a_subsession(
            workspace.path(),
            subsession,
            "Fix the flaky login test",
            "Fix the flaky login test in the auth suite.",
        ),
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted {
                session_id: subsession,
            },
        )))
        .expect("hear the Subsession was deleted");

    assert!(
        rendered_application_rows_at(&application, 120, 24)
            .join("\n")
            .contains("↗ Subsession: Fix the flaky login test"),
        "the row still says what the Sidekick began"
    );
    assert_eq!(
        press_text(&mut application, "Subsession: Fix"),
        ApplicationTransition::Continue,
        "but leads nowhere, since there is no Session left to open"
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SubsessionOpen,
            )))
            .expect("invoke the command naming no Session"),
        ApplicationTransition::Continue,
    );
}

#[test]
fn a_subsession_that_cannot_be_opened_leaves_the_reader_on_the_sidekick_saying_why() {
    let workspace = workspace_dir();
    let subsession = SessionId::new();
    let mut application = attached(
        began_a_subsession(
            workspace.path(),
            subsession,
            "Fix the flaky login test",
            "Fix the flaky login test in the auth suite.",
        ),
        workspace.path(),
    );
    let reference = SessionReference::new(Outlook::Local, subsession);
    assert_eq!(
        press_text(&mut application, "Subsession: Fix"),
        ApplicationTransition::ViewAndAttachSession(reference.clone()),
    );
    application
        .handle_event(ApplicationEvent::OriginSessionAttachFailed {
            reference,
            error: "The Session does not exist on this Suru server.".to_owned(),
        })
        .expect("deliver the failed attach");

    let text = rendered_application_rows_at(&application, 160, 24).join("\n");
    assert!(
        text.contains("↗ Subsession: Fix the flaky login test"),
        "the reader is left on the Sidekick's Transcript: {text}"
    );
    assert!(
        text.contains("Could not open the Subsession: The Session does not exist"),
        "told why the Subsession did not open: {text}"
    );
}
