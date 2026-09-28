//! Pasting from the clipboard into the composer: Ctrl+V reads the clipboard
//! through the run loop, an image is uploaded and stands in the draft as an
//! `[Image N]` label, and every failure says why in a Notice. The clipboard
//! and the upload are answered here as the run loop would answer them.

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Color;
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ApprovalOutcome, ApprovalSubject, AttachmentBinding, AttachmentDescriptor,
        AttachmentId, AttachmentKind, Outlook, PromptDelivery, SessionId, SessionTimestamp,
        TextSpan,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, ClipboardRead, CommandId, PasteId,
        SemanticCommandId,
    },
};

use crate::support::{
    add_activity, approval_activity, click_mouse, connected_application, enter_active_session,
    invoke, key, rendered_application_buffer, rendered_application_rows,
    rendered_application_rows_at, text_position, type_terminal_text, workspace_dir,
};

fn press(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("deliver a key press")
}

fn ctrl_v(application: &mut Application) -> ApplicationTransition {
    press(application, KeyCode::Char('v'), KeyModifiers::CONTROL)
}

/// Presses Ctrl+V and answers the paste it began.
fn read_clipboard(application: &mut Application) -> PasteId {
    match ctrl_v(application) {
        ApplicationTransition::ReadClipboard(paste) => paste,
        other => panic!("Ctrl+V in the composer reads the clipboard, not {other:?}"),
    }
}

fn answer_read(
    application: &mut Application,
    paste: PasteId,
    read: ClipboardRead,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::ClipboardRead { paste, read })
        .expect("answer the clipboard read")
}

fn png(tag: &str) -> Vec<u8> {
    [b"\x89PNG\r\n\x1a\n".as_slice(), tag.as_bytes()].concat()
}

fn descriptor(tag: &str, width: u32, height: u32, byte_length: u64) -> AttachmentDescriptor {
    AttachmentDescriptor {
        id: AttachmentId::new(format!("{tag}-hash")),
        kind: AttachmentKind::Image { width, height },
        mime_type: "image/png".to_owned(),
        byte_length,
    }
}

/// Pastes an image the whole way: Ctrl+V, the clipboard holding `tag`'s
/// PNG, and the Server storing it as `stored`. Answers what the upload was
/// asked to reach.
fn paste_image(application: &mut Application, tag: &str, stored: AttachmentDescriptor) -> Outlook {
    let paste = read_clipboard(application);
    let ApplicationTransition::UploadAttachment {
        paste: uploading,
        origin,
        png: uploaded,
    } = answer_read(application, paste, ClipboardRead::Image { png: png(tag) })
    else {
        panic!("a clipboard image is uploaded");
    };
    assert_eq!(uploading, paste);
    assert_eq!(uploaded, png(tag));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AttachmentUploaded {
                paste,
                descriptor: stored,
            })
            .expect("answer the upload"),
        ApplicationTransition::Continue
    );
    origin
}

fn submit(application: &mut Application, command: CommandId) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Command(command))
        .expect("submit the draft")
}

fn bound(tag: &str, label: &str, span: std::ops::Range<usize>) -> AttachmentBinding {
    AttachmentBinding {
        attachment_id: AttachmentId::new(format!("{tag}-hash")),
        label: label.to_owned(),
        span: TextSpan::from(span),
    }
}

fn composer_row(application: &Application) -> String {
    rendered_application_rows(application)
        .into_iter()
        .find(|row| row.contains("[Image"))
        .unwrap_or_default()
}

#[test]
fn a_pasted_image_is_uploaded_and_labelled_with_numbers_a_draft_never_reuses() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());

    let origin = paste_image(&mut application, "first", descriptor("first", 1280, 720, 1));
    assert_eq!(origin, Outlook::Local);
    paste_image(
        &mut application,
        "second",
        descriptor("second", 640, 480, 2),
    );
    assert!(composer_row(&application).contains("[Image 1] [Image 2]"));

    // Delete [Image 1] from its start, then paste again where it stood.
    press(&mut application, KeyCode::Home, KeyModifiers::NONE);
    press(&mut application, KeyCode::Delete, KeyModifiers::NONE);
    paste_image(&mut application, "third", descriptor("third", 16, 16, 3));

    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session");
    };
    assert_eq!(request.prompt.text, "[Image 3]  [Image 2] ");
    assert_eq!(
        request.prompt.attachments,
        vec![
            bound("third", "[Image 3]", 0..9),
            bound("second", "[Image 2]", 11..20),
        ]
    );

    // The next Prompt starts counting again. (The Provisional Session's
    // Transcript draws the submitted labels above it.)
    paste_image(&mut application, "fourth", descriptor("fourth", 16, 16, 4));
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("[Image 1]")
    );
}

/// The rows of the user Message block in the Transcript reading `first`.
fn transcript_block(application: &Application, first: &str) -> Vec<String> {
    let rows = rendered_application_rows(application);
    let start = rows
        .iter()
        .position(|row| row.contains(&format!("┃ {first}")))
        .unwrap_or_else(|| panic!("the Message is drawn: {rows:#?}"));
    rows[start..]
        .iter()
        .take_while(|row| row.trim_start().starts_with('┃'))
        .map(|row| row.trim().to_owned())
        .collect()
}

#[test]
fn a_submitted_prompt_lists_its_attachments_before_and_after_its_session_arrives() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "What is ");
    let stored = descriptor("wide", 1280, 720, 312 * 1024);
    paste_image(&mut application, "wide", stored.clone());
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the Landing's Prompt begins a Session");
    };

    // The Provisional Session describes what this client uploaded.
    let claimed = transcript_block(&application, "What is");
    assert_eq!(
        claimed,
        vec![
            "┃ What is [Image 1]",
            "┃ Image 1 · PNG · 1280×720 · 312 KiB"
        ]
    );

    // The Session arrives describing what it binds, and the row stands.
    let mut created = crate::provisional_session::created_session_snapshot(
        SessionId::new(),
        &request.prompt,
        workspace.path(),
        SessionTimestamp::now(),
    );
    created.attachments = vec![stored];
    application
        .handle_event(ApplicationEvent::SessionCreated(created))
        .expect("take the created Session");
    assert_eq!(transcript_block(&application, "What is"), claimed);
}

#[test]
fn a_prompt_of_only_a_label_is_not_whitespace() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    paste_image(&mut application, "only", descriptor("only", 2, 2, 70));

    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("a label alone is a Prompt");
    };
    assert_eq!(request.prompt.text, "[Image 1] ");
    assert_eq!(
        request.prompt.attachments,
        vec![bound("only", "[Image 1]", 0..9)]
    );
}

#[test]
fn the_label_moves_selects_and_deletes_as_one_unit() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "a ");
    paste_image(&mut application, "unit", descriptor("unit", 2, 2, 70));
    type_terminal_text(&mut application, "b");
    assert!(composer_row(&application).contains("a [Image 1] b"));

    // Left steps over the whole label, and typing at either edge leaves it
    // bound.
    key(&mut application, KeyCode::Left);
    key(&mut application, KeyCode::Left);
    key(&mut application, KeyCode::Left);
    type_terminal_text(&mut application, "<");
    key(&mut application, KeyCode::Right);
    type_terminal_text(&mut application, ">");
    assert!(composer_row(&application).contains("a <[Image 1]> b"));

    // Shift+Left from the label's end selects all of it, and typing replaces
    // it whole.
    key(&mut application, KeyCode::Left);
    press(&mut application, KeyCode::Left, KeyModifiers::SHIFT);
    type_terminal_text(&mut application, "x");
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is submitted");
    };
    assert_eq!(request.prompt.text, "a <x> b");
    assert!(request.prompt.attachments.is_empty());
}

#[test]
fn a_click_inside_the_label_places_the_cursor_at_its_nearer_edge() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "a ");
    paste_image(&mut application, "click", descriptor("click", 2, 2, 70));
    type_terminal_text(&mut application, "b");

    for (into, typed, drawn) in [(2, "<", "a <[Image 1] b"), (7, ">", "a <[Image 1]> b")] {
        let buffer = rendered_application_buffer(&application, 100, 30);
        let (column, row) = text_position(&buffer, "[Image 1]");
        click_mouse(
            &mut application,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: column + into,
                row,
                modifiers: KeyModifiers::NONE,
            },
        )
        .expect("click inside the label");
        type_terminal_text(&mut application, typed);
        let rows = rendered_application_rows_at(&application, 100, 30).join("\n");
        assert!(rows.contains(drawn), "{rows}");
    }
    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is submitted");
    };
    assert_eq!(
        request.prompt.attachments,
        vec![bound("click", "[Image 1]", 3..12)]
    );
}

#[test]
fn backspace_or_delete_into_the_label_drops_it_and_its_binding() {
    let workspace = workspace_dir();
    for (deletion, moves) in [(KeyCode::Backspace, 2), (KeyCode::Delete, 3)] {
        let mut application = connected_application(workspace.path());
        type_terminal_text(&mut application, "a ");
        paste_image(&mut application, "gone", descriptor("gone", 2, 2, 70));
        type_terminal_text(&mut application, "b");
        for _ in 0..moves {
            key(&mut application, KeyCode::Left);
        }
        key(&mut application, deletion);
        let ApplicationTransition::CreateSession(request) =
            submit(&mut application, CommandId::SubmitSteer)
        else {
            panic!("the draft is submitted");
        };
        assert_eq!(request.prompt.text, "a  b", "{deletion:?}");
        assert!(request.prompt.attachments.is_empty(), "{deletion:?}");
    }
}

#[test]
fn a_dimmed_line_beneath_the_text_describes_each_attachment_until_its_label_goes() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "Compare ");
    paste_image(
        &mut application,
        "wide",
        descriptor("wide", 1280, 720, 312 * 1024),
    );
    paste_image(
        &mut application,
        "tall",
        descriptor("tall", 90, 1600, 2_411_725),
    );

    let rows = rendered_application_rows(&application);
    let text = rows
        .iter()
        .position(|row| row.contains("Compare [Image 1] [Image 2]"))
        .expect("the draft is drawn");
    assert!(
        rows[text + 1].contains("Image 1 · PNG · 1280×720 · 312 KiB"),
        "{rows:#?}"
    );
    assert!(
        rows[text + 2].contains("Image 2 · PNG · 90×1600 · 2.3 MiB"),
        "{rows:#?}"
    );
    assert!(
        rows[text + 3].contains('└'),
        "the lines stand inside the composer: {rows:#?}"
    );

    // The label is accented as a Skill Invocation is; its line is not.
    let buffer = rendered_application_buffer(&application, 80, 24);
    let label = buffer
        .content()
        .windows("[Image 1]".len())
        .find(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == "[Image 1]")
        .expect("the label is drawn");
    assert!(label.iter().all(|cell| cell.fg == Color::Cyan));

    key(&mut application, KeyCode::Backspace);
    key(&mut application, KeyCode::Backspace);
    let rows = rendered_application_rows(&application).join("\n");
    assert!(
        rows.contains("Image 1 · PNG · 1280×720 · 312 KiB"),
        "{rows}"
    );
    assert!(!rows.contains("Image 2 ·"), "{rows}");
}

#[test]
fn ctrl_v_with_text_pastes_it_and_with_nothing_changes_nothing() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "Say ");

    let paste = read_clipboard(&mut application);
    assert_eq!(
        answer_read(
            &mut application,
            paste,
            ClipboardRead::Text("hello\nthere".to_owned())
        ),
        ApplicationTransition::Continue
    );
    let paste = read_clipboard(&mut application);
    let before = rendered_application_rows(&application);
    assert_eq!(
        answer_read(&mut application, paste, ClipboardRead::Empty),
        ApplicationTransition::Continue
    );
    assert_eq!(rendered_application_rows(&application), before);

    let ApplicationTransition::CreateSession(request) =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the draft is submitted");
    };
    assert_eq!(request.prompt.text, "Say hello\nthere");
}

#[test]
fn an_empty_bracketed_paste_reads_the_clipboard() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    assert!(matches!(
        application
            .handle_terminal_event(InputEvent::Paste(String::new()))
            .expect("deliver an empty paste"),
        ApplicationTransition::ReadClipboard(_)
    ));
    // A paste with text in it is still the text.
    application
        .handle_terminal_event(InputEvent::Paste("typed".to_owned()))
        .expect("deliver a paste");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("typed")
    );
}

/// How a failing paste is answered once Ctrl+V has begun it.
type FailingAnswer = Box<dyn Fn(&mut Application, PasteId) -> ApplicationTransition>;

#[test]
fn every_failed_paste_says_why_and_leaves_the_draft_as_it_was() {
    let workspace = workspace_dir();
    let too_large = 7_549_747;
    let cases: Vec<(&str, FailingAnswer)> = vec![
        (
            "Could not read the clipboard: held by another party",
            Box::new(|application, paste| {
                answer_read(
                    application,
                    paste,
                    ClipboardRead::Failed {
                        reason: "held by another party.".to_owned(),
                    },
                )
            }),
        ),
        (
            "An image may be at most 5 MiB, and this one is 7.2 MiB",
            Box::new(move |application, paste| {
                answer_read(
                    application,
                    paste,
                    ClipboardRead::Image {
                        png: vec![0; too_large],
                    },
                )
            }),
        ),
        (
            "The clipboard's BMP image cannot be attached",
            Box::new(|application, paste| {
                answer_read(
                    application,
                    paste,
                    ClipboardRead::Unsupported {
                        format: "BMP".to_owned(),
                    },
                )
            }),
        ),
        (
            "Only PNG, JPEG, GIF, and WebP images can be attached",
            Box::new(|application, paste| {
                let ApplicationTransition::UploadAttachment { .. } =
                    answer_read(application, paste, ClipboardRead::Image { png: png("x") })
                else {
                    panic!("the image is uploaded before the Server refuses it");
                };
                application
                    .handle_event(ApplicationEvent::AttachmentUploadFailed {
                        paste,
                        reason: "Only PNG, JPEG, GIF, and WebP images can be attached".to_owned(),
                    })
                    .expect("answer the refusal")
            }),
        ),
    ];
    for (notice, answer) in cases {
        let mut application = connected_application(workspace.path());
        type_terminal_text(&mut application, "Keep this");
        let before = rendered_application_rows(&application);
        let paste = read_clipboard(&mut application);
        assert_eq!(
            answer(&mut application, paste),
            ApplicationTransition::Continue
        );

        let rows = rendered_application_rows(&application);
        let shown = rows
            .iter()
            .find(|row| row.contains(notice))
            .unwrap_or_else(|| panic!("the Notice says {notice:?}: {rows:#?}"));
        assert!(shown.contains("see the Log"), "{shown}");
        assert!(rows.iter().any(|row| row.contains("Keep this")));
        assert!(!rows.join("\n").contains("[Image"));

        // The reader's next key dismisses it, and the draft is as it was.
        key(&mut application, KeyCode::End);
        assert_eq!(rendered_application_rows(&application), before, "{notice}");
    }
}

#[test]
fn a_windows_clipboard_powershell_could_not_read_raises_the_read_failure_notice() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    type_terminal_text(&mut application, "Keep this");
    let paste = read_clipboard(&mut application);
    assert_eq!(
        answer_read(
            &mut application,
            paste,
            ClipboardRead::Failed {
                reason: "powershell.exe timed out on the Windows clipboard after 5s".to_owned(),
            },
        ),
        ApplicationTransition::Continue
    );

    let rows = rendered_application_rows_at(&application, 120, 30);
    let shown = rows
        .iter()
        .find(|row| {
            row.contains(
                "Could not read the clipboard: powershell.exe timed out on the Windows clipboard",
            )
        })
        .unwrap_or_else(|| panic!("the read failure is a Notice: {rows:#?}"));
    assert!(shown.contains("see the Log"), "{shown}");
    assert!(rows.iter().any(|row| row.contains("Keep this")));
    assert!(!rows.join("\n").contains("[Image"));
}

#[test]
fn an_eleventh_image_is_refused_before_it_is_uploaded() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    for number in 1..=10 {
        paste_image(
            &mut application,
            &format!("image-{number}"),
            descriptor(&format!("image-{number}"), 2, 2, 70),
        );
    }
    let paste = read_clipboard(&mut application);
    assert_eq!(
        answer_read(
            &mut application,
            paste,
            ClipboardRead::Image {
                png: png("eleventh")
            }
        ),
        ApplicationTransition::Continue
    );
    let rows = rendered_application_rows(&application).join("\n");
    assert!(
        rows.contains("A Prompt may carry at most 10 Attachments"),
        "{rows}"
    );
    assert!(!rows.contains("[Image 11]"));

    // An eleventh counted while ten are still uploading is refused too.
    let mut application = connected_application(workspace.path());
    let pastes = (0..10)
        .map(|_| {
            let paste = read_clipboard(&mut application);
            assert!(matches!(
                answer_read(
                    &mut application,
                    paste,
                    ClipboardRead::Image { png: png("a") }
                ),
                ApplicationTransition::UploadAttachment { .. }
            ));
            paste
        })
        .collect::<Vec<_>>();
    let paste = read_clipboard(&mut application);
    assert_eq!(
        answer_read(
            &mut application,
            paste,
            ClipboardRead::Image { png: png("b") }
        ),
        ApplicationTransition::Continue
    );
    assert!(pastes.iter().all(|pending| *pending != paste));
}

#[test]
fn a_steer_and_a_queued_prompt_carry_their_bindings() {
    let workspace = workspace_dir();
    for (command, delivery) in [
        (CommandId::SubmitSteer, PromptDelivery::Steer),
        (CommandId::SubmitQueue, PromptDelivery::Queue),
    ] {
        let mut application = connected_application(workspace.path());
        let (session_id, _, _) = enter_active_session(&mut application, workspace.path());
        type_terminal_text(&mut application, "See ");
        let origin = paste_image(&mut application, "shot", descriptor("shot", 2, 2, 70));
        assert_eq!(origin, Outlook::Local);

        let ApplicationTransition::AdmitPrompt { session, request } =
            submit(&mut application, command)
        else {
            panic!("a Prompt into a working Session is admitted");
        };
        assert_eq!(session.session_id, session_id);
        assert_eq!(request.delivery, delivery);
        assert_eq!(request.prompt.text, "See [Image 1] ");
        assert_eq!(
            request.prompt.attachments,
            vec![bound("shot", "[Image 1]", 4..13)]
        );
    }
}

#[test]
fn ctrl_v_outside_the_composer_does_nothing() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    invoke(&mut application, SemanticCommandId::ThemeList);
    let before = rendered_application_rows(&application);
    assert_eq!(ctrl_v(&mut application), ApplicationTransition::Continue);
    assert_eq!(
        invoke(&mut application, SemanticCommandId::ComposerClipboardPaste),
        ApplicationTransition::Continue
    );
    assert_eq!(rendered_application_rows(&application), before);

    // An open Approval has the keys, and neither Ctrl+V nor the empty paste
    // a terminal sends for it reaches the draft behind the panel.
    let mut application = connected_application(workspace.path())
        .with_intervention_arming_delay(std::time::Duration::ZERO);
    let (_, mut snapshot, turn_id) = enter_active_session(&mut application, workspace.path());
    let activity = approval_activity(
        turn_id,
        ApprovalSubject::Network {
            host_or_url: "https://api.example.test/v1".into(),
        },
        Some("Download build metadata"),
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval { approval, .. } = &activity else {
        unreachable!()
    };
    snapshot.pending_approvals = vec![approval.id];
    snapshot.pending_approvals_revision = snapshot.revision;
    add_activity(&mut snapshot, activity);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("present the Approval");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Approval · Choose Decision")
    );
    assert_eq!(ctrl_v(&mut application), ApplicationTransition::Continue);
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Paste(String::new()))
            .expect("deliver an empty paste"),
        ApplicationTransition::Continue
    );
}

#[test]
fn an_upload_landing_after_its_draft_was_submitted_labels_the_next_prompt() {
    // An upload that lands once its draft was submitted belongs to the next
    // Prompt written there, labelled from 1.
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    let (_, _, _) = enter_active_session(&mut application, workspace.path());
    type_terminal_text(&mut application, "Now");
    let paste = read_clipboard(&mut application);
    let ApplicationTransition::UploadAttachment { .. } = answer_read(
        &mut application,
        paste,
        ClipboardRead::Image { png: png("late") },
    ) else {
        panic!("the image is uploaded");
    };
    let ApplicationTransition::AdmitPrompt { request, .. } =
        submit(&mut application, CommandId::SubmitSteer)
    else {
        panic!("the text is submitted without waiting on the upload");
    };
    assert!(request.prompt.attachments.is_empty());
    application
        .handle_event(ApplicationEvent::AttachmentUploaded {
            paste,
            descriptor: descriptor("late", 2, 2, 70),
        })
        .expect("answer the upload");
    assert!(composer_row(&application).contains("[Image 1]"));
}
