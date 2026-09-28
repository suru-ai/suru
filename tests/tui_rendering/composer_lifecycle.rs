//! Attachments across the composer's lifecycle edges: a draft carrying one
//! survives a Session switch and a rejected admission whole, composer history
//! recalls a Prompt's Attachments with its text and asks whether they are still
//! stored, and a label whose Attachment its Server no longer holds, or never
//! held, is demoted to plain text with a Notice naming it.

use crossterm::event::KeyCode;
use ratatui::style::Color;
use suru::{
    protocol::{AttachmentId, Outlook, PromptId, SessionId, SessionReference, SessionTimestamp},
    tui::{
        Application, ApplicationEvent, ApplicationTransition, AttachmentCheckId, ClipboardRead,
        CommandId, SemanticCommandId,
    },
};

use crate::{
    image_paste::{answer_read, bound, descriptor, paste_image, png, read_clipboard, submit},
    support::{
        connected_application, enter_active_session, failed_session_snapshot, invoke, key,
        rendered_application_buffer, rendered_application_rows, type_terminal_text, workspace_dir,
    },
};

const LINE: &str = "Image 1 · PNG · 1280×720 · 312 KiB";

fn wide() -> suru::protocol::AttachmentDescriptor {
    descriptor("wide", 1280, 720, 312 * 1024)
}

/// Whether every cell drawing `label` in the composer, the lowest place it is
/// drawn, is in the accent a bound label takes rather than plain text's.
fn label_is_accented(application: &Application, label: &str) -> bool {
    let buffer = rendered_application_buffer(application, 80, 24);
    let cells = buffer
        .content()
        .windows(label.len())
        .rfind(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == label)
        .unwrap_or_else(|| panic!("{label} is drawn"));
    cells.iter().all(|cell| cell.fg == Color::Cyan)
}

/// Opens `target` through the Session picker, listing it alone, and hydrates
/// it with `snapshot`.
fn open_through_the_picker(
    application: &mut Application,
    workspace: &std::path::Path,
    target: SessionId,
    snapshot: suru::protocol::SessionSnapshot,
) {
    let ApplicationTransition::ListSessions(request) =
        invoke(application, SemanticCommandId::SessionList)
    else {
        panic!("the Session picker asks for a listing");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![crate::support::listed_session(
                target,
                "Another Session",
                workspace,
                1,
                1,
            )],
        })
        .expect("list the Sessions");
    assert_eq!(
        key(application, KeyCode::Enter),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(Outlook::Local, target))
    );
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("hydrate the Session opened");
}

#[test]
fn a_draft_with_an_attachment_is_intact_after_opening_another_session_and_returning() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let (original, snapshot, _) = enter_active_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "See ");
    paste_image(&mut application, "wide", wide());
    let before = rendered_application_rows(&application);
    assert!(before.join("\n").contains(LINE), "{before:#?}");

    let other = SessionId::new();
    open_through_the_picker(
        &mut application,
        workspace.path(),
        other,
        failed_session_snapshot(other, PromptId::new(), "Other work", workspace.path()),
    );
    let away = rendered_application_rows(&application).join("\n");
    assert!(away.contains("Other work"), "{away}");
    assert!(!away.contains("[Image 1]"), "{away}");

    open_through_the_picker(&mut application, workspace.path(), original, snapshot);
    assert_eq!(rendered_application_rows(&application), before);
    assert!(label_is_accented(&application, "[Image 1]"));

    let ApplicationTransition::AdmitPrompt { session, request } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is admitted into its Session");
    };
    assert_eq!(session.session_id, original);
    assert_eq!(request.prompt.text, "See [Image 1] ");
    assert_eq!(
        request.prompt.attachments,
        vec![bound("wide", "[Image 1]", 4..13)]
    );
}

#[test]
fn a_rejected_admission_restores_the_whole_draft_and_resubmits_its_attachments() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    enter_active_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "See ");
    paste_image(&mut application, "wide", wide());
    let before = rendered_application_rows(&application);

    let ApplicationTransition::AdmitPrompt { session, request } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is admitted into its Session");
    };
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(LINE)
    );
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session: session.clone(),
            prompt_id: request.prompt.id,
            error: "server unavailable".to_owned(),
        })
        .expect("refuse the admission");
    let restored = rendered_application_rows(&application);
    let composer = |rows: &[String]| {
        rows.iter()
            .filter(|row| row.contains("[Image") || row.contains("Image 1 ·"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(composer(&restored), composer(&before), "{restored:#?}");
    assert!(label_is_accented(&application, "[Image 1]"));

    // Resubmitted unchanged, it is the very Prompt it was.
    let ApplicationTransition::AdmitPrompt { request: retry, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the restored draft is admitted again");
    };
    assert_eq!(retry.prompt, request.prompt);
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            session,
            prompt_id: retry.prompt.id,
            error: "still unavailable".to_owned(),
        })
        .expect("refuse the retry");

    // A further paste never reuses the restored label's number.
    key(&mut application, KeyCode::End);
    paste_image(
        &mut application,
        "tall",
        descriptor("tall", 90, 1600, 2_411_725),
    );
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is admitted");
    };
    assert_eq!(request.prompt.text, "See [Image 1] [Image 2] ");
    assert_eq!(
        request.prompt.attachments,
        vec![
            bound("wide", "[Image 1]", 4..13),
            bound("tall", "[Image 2]", 14..23),
        ]
    );
}

/// Sends "See [Image 1] " from the Landing and lets the Session it begins
/// arrive, which is what puts the Prompt in that composer's history.
fn send_a_prompt_with_an_image(application: &mut Application, workspace: &std::path::Path) {
    type_terminal_text(application, "See ");
    paste_image(application, "wide", wide());
    let ApplicationTransition::CreateSession(request) = submit(application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session");
    };
    let mut created = crate::provisional_session::created_session_snapshot(
        SessionId::new(),
        &request.prompt,
        workspace,
        SessionTimestamp::now(),
    );
    created.attachments = vec![wide()];
    application
        .handle_event(ApplicationEvent::SessionCreated(created))
        .expect("take the created Session");
}

/// Presses Up in an empty composer, recalling the latest Prompt, and answers
/// the check the recall asks for.
fn recall(application: &mut Application) -> AttachmentCheckId {
    match key(application, KeyCode::Up) {
        ApplicationTransition::CheckAttachments {
            check,
            origin,
            attachments,
        } => {
            assert_eq!(origin, Outlook::Local);
            assert_eq!(attachments, vec![AttachmentId::new("wide-hash")]);
            check
        }
        other => panic!("recalling an Attachment asks whether it is still stored, not {other:?}"),
    }
}

fn answer_check(
    application: &mut Application,
    check: AttachmentCheckId,
    result: Result<Vec<AttachmentId>, String>,
) {
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AttachmentsChecked { check, result })
            .expect("answer the check"),
        ApplicationTransition::Continue
    );
}

/// The row carrying the draft's text, read in the composer at the bottom of
/// the frame rather than in the Transcript above it.
fn draft_rows(application: &Application) -> Vec<String> {
    let rows = rendered_application_rows(application);
    let top = rows
        .iter()
        .rposition(|row| row.contains('┌') || row.contains('╭') || row.contains('▀'))
        .unwrap_or(0);
    rows[top..].to_vec()
}

#[test]
fn history_recalls_a_sent_prompt_with_its_attachments_and_resending_sends_them_again() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());

    let check = recall(&mut application);
    let recalled = draft_rows(&application).join("\n");
    assert!(recalled.contains("See [Image 1]"), "{recalled}");
    assert!(recalled.contains(LINE), "{recalled}");
    assert!(label_is_accented(&application, "[Image 1]"));

    // Every Attachment is still stored, so nothing changes.
    let before = rendered_application_rows(&application);
    answer_check(&mut application, check, Ok(Vec::new()));
    assert_eq!(rendered_application_rows(&application), before);

    // A paste after the recall takes the next number.
    key(&mut application, KeyCode::End);
    paste_image(
        &mut application,
        "tall",
        descriptor("tall", 90, 1600, 2_411_725),
    );
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the recalled Prompt is admitted into the Session");
    };
    assert_eq!(request.prompt.text, "See [Image 1] [Image 2] ");
    assert_eq!(
        request.prompt.attachments,
        vec![
            bound("wide", "[Image 1]", 4..13),
            bound("tall", "[Image 2]", 14..23),
        ]
    );
}

#[test]
fn a_recalled_prompt_whose_attachment_was_swept_keeps_its_label_as_plain_text() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());

    let check = recall(&mut application);
    answer_check(
        &mut application,
        check,
        Ok(vec![AttachmentId::new("wide-hash")]),
    );
    let rows = rendered_application_rows(&application);
    let notice = rows
        .iter()
        .find(|row| row.contains("Image 1 is no longer on the Server; its label stays as text"))
        .unwrap_or_else(|| panic!("a Notice names the missing image: {rows:#?}"));
    assert!(notice.contains("see the Log"), "{notice}");
    let draft = draft_rows(&application).join("\n");
    assert!(draft.contains("See [Image 1]"), "{draft}");
    assert!(
        !draft.contains(LINE),
        "a demoted label has no line: {draft}"
    );
    assert!(!label_is_accented(&application, "[Image 1]"));

    // The reader's next key dismisses the Notice, and the label is text.
    key(&mut application, KeyCode::End);
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("no longer on the Server")
    );
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the recalled text is admitted");
    };
    assert_eq!(request.prompt.text, "See [Image 1] ");
    assert!(request.prompt.attachments.is_empty());
}

#[test]
fn a_check_answered_after_its_label_was_edited_away_or_that_failed_changes_nothing() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());

    // The check could not be made: the binding stands, and admission will
    // say whether the Attachment is there.
    let check = recall(&mut application);
    let before = rendered_application_rows(&application);
    answer_check(
        &mut application,
        check,
        Err("connection refused".to_owned()),
    );
    assert_eq!(rendered_application_rows(&application), before);
    assert!(label_is_accented(&application, "[Image 1]"));

    // The label the check was about is gone by the time it is answered.
    key(&mut application, KeyCode::Down);
    let check = recall(&mut application);
    key(&mut application, KeyCode::Backspace);
    key(&mut application, KeyCode::Backspace);
    type_terminal_text(&mut application, "[Image 1]");
    let before = rendered_application_rows(&application);
    answer_check(
        &mut application,
        check,
        Ok(vec![AttachmentId::new("wide-hash")]),
    );
    assert_eq!(rendered_application_rows(&application), before);
    assert!(
        !before.join("\n").contains("no longer on the Server"),
        "{before:#?}"
    );
}

/// Whether the composer shows `label` bound, with the dimmed line beneath
/// it, and no Notice says its image is gone.
fn stands_bound(application: &Application, line: &str) -> bool {
    let rows = rendered_application_rows(application).join("\n");
    label_is_accented(application, "[Image 1]")
        && rows.contains(line)
        && !rows.contains("no longer on the Server")
}

#[test]
fn a_check_answered_once_the_draft_was_cleared_leaves_a_fresh_paste_of_the_image_bound() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());
    let check = recall(&mut application);

    // Cleared, the draft counts its labels from 1 again, and the same image
    // pasted anew stands as the very label and id the recall bound.
    application
        .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
        .expect("clear the draft");
    paste_image(&mut application, "wide", wide());
    answer_check(
        &mut application,
        check,
        Ok(vec![AttachmentId::new("wide-hash")]),
    );
    assert!(stands_bound(&application, LINE));
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the fresh paste is admitted");
    };
    assert_eq!(
        request.prompt.attachments,
        vec![bound("wide", "[Image 1]", 0..9)]
    );
}

#[test]
fn a_check_answered_once_the_recall_was_sent_leaves_a_fresh_paste_of_the_image_bound() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());
    let check = recall(&mut application);
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the recalled Prompt is admitted");
    };
    assert_eq!(
        request.prompt.attachments,
        vec![bound("wide", "[Image 1]", 4..13)]
    );

    paste_image(&mut application, "wide", wide());
    answer_check(
        &mut application,
        check,
        Ok(vec![AttachmentId::new("wide-hash")]),
    );
    assert!(stands_bound(&application, LINE));
}

/// Leaves the Landing with "Old [Image 1]" bound to `wide` as its draft and
/// "Newer [Image 1]" bound to `tall` in its history: the Landing's Prompt is
/// refused after the reader went back to the Landing and wrote there, so the
/// refused Prompt comes back as the draft and what they wrote waits in
/// history.
fn landing_with_an_image_in_its_draft_and_its_history(application: &mut Application) {
    type_terminal_text(application, "Old ");
    paste_image(application, "wide", wide());
    let ApplicationTransition::CreateSession(request) = submit(application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session");
    };
    invoke(application, SemanticCommandId::SessionNew);
    type_terminal_text(application, "Newer ");
    paste_image(application, "tall", tall());
    application
        .handle_event(ApplicationEvent::SessionCreationFailed {
            prompt_id: request.prompt.id,
            code: None,
            error: "Provider unavailable".to_owned(),
        })
        .expect("refuse the Session");
    let rows = draft_rows(application).join("\n");
    assert!(rows.contains("Old [Image 1]"), "{rows}");
}

fn tall() -> suru::protocol::AttachmentDescriptor {
    descriptor("tall", 90, 1600, 2_411_725)
}

#[test]
fn turning_the_outlook_leaves_the_landings_history_and_set_aside_draft_unbound_too() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    landing_with_an_image_in_its_draft_and_its_history(&mut application);

    // Up sets the draft aside and recalls the history entry.
    let ApplicationTransition::CheckAttachments { attachments, .. } =
        key(&mut application, KeyCode::Up)
    else {
        panic!("the recall asks after its Attachment");
    };
    assert_eq!(attachments, vec![AttachmentId::new("tall-hash")]);

    // The Notice names what the draft shows; the draft set aside and the
    // history lose their bindings as quietly.
    turn_toward_studio(&mut application);
    let rows = rendered_application_rows(&application).join("\n");
    assert!(
        rows.contains("! Image 1 is on another Server; its label stays as text"),
        "{rows}"
    );
    assert!(rows.contains("Newer [Image 1]"), "{rows}");
    assert!(!label_is_accented(&application, "[Image 1]"));

    // Down brings back the draft set aside, its label as text.
    assert_eq!(
        key(&mut application, KeyCode::Down),
        ApplicationTransition::Continue
    );
    let rows = draft_rows(&application).join("\n");
    assert!(rows.contains("Old [Image 1]"), "{rows}");
    assert!(!rows.contains("Image 1 ·"), "{rows}");
    assert!(!label_is_accented(&application, "[Image 1]"));

    // A recall after the turn brings no Attachment back, and asks nothing.
    assert_eq!(
        key(&mut application, KeyCode::Up),
        ApplicationTransition::Continue
    );
    let rows = draft_rows(&application).join("\n");
    assert!(rows.contains("Newer [Image 1]"), "{rows}");
    assert!(!label_is_accented(&application, "[Image 1]"));
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session on the Remote");
    };
    assert_eq!(request.prompt.text, "Newer [Image 1] ");
    assert!(request.prompt.attachments.is_empty());
}

#[test]
fn turning_the_outlook_leaves_a_landing_drafts_attachments_behind_as_plain_text() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "See ");
    assert_eq!(
        paste_image(&mut application, "wide", wide()),
        Outlook::Local
    );

    turn_toward_studio(&mut application);
    let rows = rendered_application_rows(&application);
    let notice = rows
        .iter()
        .find(|row| row.contains("Image 1 is on another Server; its label stays as text"))
        .unwrap_or_else(|| panic!("a Notice names the image left behind: {rows:#?}"));
    assert!(notice.contains("see the Log"), "{notice}");
    let joined = rows.join("\n");
    assert!(joined.contains("See [Image 1]"), "{joined}");
    assert!(!joined.contains(LINE), "{joined}");
    assert!(!label_is_accented(&application, "[Image 1]"));

    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session on the Remote");
    };
    assert_eq!(request.prompt.text, "See [Image 1] ");
    assert!(request.prompt.attachments.is_empty());
}

#[test]
fn a_refused_prompt_the_outlook_turn_hands_back_to_the_landing_leaves_its_attachments_behind() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "See ");
    paste_image(&mut application, "wide", wide());
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session");
    };
    application
        .handle_event(ApplicationEvent::SessionCreationFailed {
            prompt_id: request.prompt.id,
            code: None,
            error: "Provider unavailable".to_owned(),
        })
        .expect("refuse the Session");

    // Turning away abandons the failed Provisional Session, whose Prompt
    // comes back to the Landing with its label as text.
    turn_toward_studio(&mut application);
    let rows = rendered_application_rows(&application).join("\n");
    assert!(
        rows.contains("Image 1 is on another Server; its label stays as text"),
        "{rows}"
    );
    assert!(rows.contains("See [Image 1]"), "{rows}");
    assert!(!label_is_accented(&application, "[Image 1]"));
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session on the Remote");
    };
    assert_eq!(request.prompt.text, "See [Image 1] ");
    assert!(request.prompt.attachments.is_empty());
}

#[test]
fn a_paste_still_uploading_when_the_outlook_turns_is_not_labelled_into_the_landing() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "See ");
    let paste = read_clipboard(&mut application);
    let ApplicationTransition::UploadAttachment { origin, .. } = answer_read(
        &mut application,
        paste,
        ClipboardRead::Image { png: png("late") },
    ) else {
        panic!("the image is uploaded");
    };
    assert_eq!(origin, Outlook::Local);

    turn_toward_studio(&mut application);
    application
        .handle_event(ApplicationEvent::AttachmentUploaded {
            paste,
            descriptor: descriptor("late", 2, 2, 70),
        })
        .expect("answer the upload");
    let rows = rendered_application_rows(&application).join("\n");
    assert!(!rows.contains("[Image"), "{rows}");
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session on the Remote");
    };
    assert!(request.prompt.attachments.is_empty());
}

/// Turns the Outlook toward the Remote `studio` through the Connect overlay,
/// which leaves the Landing's draft where it is.
fn turn_toward_studio(application: &mut Application) {
    assert_eq!(
        invoke(application, SemanticCommandId::ConnectOpen),
        ApplicationTransition::BeginConnecting
    );
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![
            suru::protocol::Remote {
                name: "studio".to_owned(),
                fingerprint: "studio-fingerprint".to_owned(),
                addresses: vec!["10.0.0.8:7777".parse().expect("parse the Remote address")],
                status: suru::protocol::RemoteStatus::Available,
            },
        ]))
        .expect("list the paired Remotes");
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(suru::protocol::RemoteHealth {
                protocol_version: Some(suru::protocol::PROTOCOL_VERSION),
                status: suru::protocol::RemoteStatus::Available,
            }),
        })
        .expect("probe the Remote");
    key(application, KeyCode::Down);
    let ApplicationTransition::TurnOutlook { outlook, .. } = key(application, KeyCode::Enter)
    else {
        panic!("choosing the Remote turns the Outlook");
    };
    assert_eq!(outlook, Outlook::Remote("studio".to_owned()));
}

#[test]
fn an_upload_landing_once_the_landing_draft_became_a_session_labels_it_or_is_dropped_quietly() {
    // Landing before the Session arrives, the upload labels the draft the
    // Session inherits.
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "Now");
    let paste = read_clipboard(&mut application);
    let ApplicationTransition::UploadAttachment { .. } = answer_read(
        &mut application,
        paste,
        ClipboardRead::Image { png: png("early") },
    ) else {
        panic!("the image is uploaded");
    };
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the text is submitted without waiting on the upload");
    };
    assert!(request.prompt.attachments.is_empty());
    application
        .handle_event(ApplicationEvent::AttachmentUploaded {
            paste,
            descriptor: descriptor("early", 2, 2, 70),
        })
        .expect("answer the upload");
    application
        .handle_event(ApplicationEvent::SessionCreated(
            crate::provisional_session::created_session_snapshot(
                SessionId::new(),
                &request.prompt,
                workspace.path(),
                SessionTimestamp::now(),
            ),
        ))
        .expect("take the created Session");
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the next Prompt is admitted into the Session");
    };
    assert_eq!(request.prompt.text, "[Image 1] ");
    assert_eq!(
        request.prompt.attachments,
        vec![bound("early", "[Image 1]", 0..9)]
    );

    // Landing after the Session arrived, it has no draft left to label and
    // is dropped without a word; only a failed upload says anything.
    for failed in [false, true] {
        let mut application = connected_application(workspace.path());
        type_terminal_text(&mut application, "Now");
        let paste = read_clipboard(&mut application);
        let ApplicationTransition::UploadAttachment { .. } = answer_read(
            &mut application,
            paste,
            ClipboardRead::Image { png: png("late") },
        ) else {
            panic!("the image is uploaded");
        };
        let ApplicationTransition::CreateSession(request) =
            submit(&mut application, CommandId::SubmitSteer)
        else {
            panic!("the text is submitted without waiting on the upload");
        };
        application
            .handle_event(ApplicationEvent::SessionCreated(
                crate::provisional_session::created_session_snapshot(
                    SessionId::new(),
                    &request.prompt,
                    workspace.path(),
                    SessionTimestamp::now(),
                ),
            ))
            .expect("take the created Session");
        let before = rendered_application_rows(&application);
        let answer = if failed {
            ApplicationEvent::AttachmentUploadFailed {
                paste,
                reason: "Could not upload the image".to_owned(),
            }
        } else {
            ApplicationEvent::AttachmentUploaded {
                paste,
                descriptor: descriptor("late", 2, 2, 70),
            }
        };
        application.handle_event(answer).expect("answer the upload");
        let rows = rendered_application_rows(&application);
        assert!(!rows.join("\n").contains("[Image"), "{rows:#?}");
        if failed {
            assert!(
                rows.join("\n").contains("Could not upload the image"),
                "{rows:#?}"
            );
        } else {
            assert_eq!(rows, before);
        }
    }
}

#[test]
fn history_keeps_a_draft_set_aside_for_it_with_its_attachments() {
    // A draft with an Attachment, set aside by walking into history, comes
    // back bound.
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    send_a_prompt_with_an_image(&mut application, workspace.path());
    paste_image(
        &mut application,
        "tall",
        descriptor("tall", 90, 1600, 2_411_725),
    );
    let before = rendered_application_rows(&application);
    let _ = recall(&mut application);
    assert_eq!(
        key(&mut application, KeyCode::Down),
        ApplicationTransition::Continue
    );
    assert_eq!(rendered_application_rows(&application), before);
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is admitted");
    };
    assert_eq!(
        request.prompt.attachments,
        vec![bound("tall", "[Image 1]", 0..9)]
    );
}
