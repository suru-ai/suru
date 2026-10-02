//! A user Message's Attachments in the Transcript: each label accented as a
//! Skill Invocation is, and one dimmed line per Attachment beneath the text,
//! inside the same gutter block, read from the descriptors the Session's
//! snapshot and updates carry.

use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        AttachmentBinding, AttachmentDescriptor, AttachmentId, AttachmentKind,
        CreateSessionRequest, ExecutionDirectory, InitialPrompt, MessageRole, Prompt,
        PromptDelivery, PromptId, PromptOrder, PromptStatus, SessionChange, SessionId,
        SessionRevision, SessionSnapshot, SessionUpdate, SkillId, SkillInvocation, TextSpan,
    },
    server::ServerConfig,
    tui::{Application, ApplicationEvent},
};

use crate::{
    deadlines::PROGRESS_DEADLINE,
    failing_provider_support::spawn_with_failing_provider,
    support::{
        buffer_rows, enter_active_session, enter_session, rendered_application_buffer,
        workspace_dir,
    },
};

/// Narrow enough that a descriptor line wraps inside the gutter block.
const WIDTH: u16 = 32;
const HEIGHT: u16 = 24;

fn descriptor(
    tag: &str,
    mime_type: &str,
    width: u32,
    height: u32,
    bytes: u64,
) -> AttachmentDescriptor {
    AttachmentDescriptor {
        id: AttachmentId::new(format!("{tag}-hash")),
        kind: AttachmentKind::Image { width, height },
        mime_type: mime_type.to_owned(),
        byte_length: bytes,
    }
}

fn screenshot() -> AttachmentDescriptor {
    descriptor("screenshot", "image/png", 1280, 720, 312 * 1024)
}

fn diagram() -> AttachmentDescriptor {
    descriptor("diagram", "image/gif", 320, 200, 4608)
}

/// Binds `label`, found in `text`, to the Attachment `descriptor` describes.
fn bound(descriptor: &AttachmentDescriptor, text: &str, label: &str) -> AttachmentBinding {
    let start = text.find(label).expect("the label stands in the text");
    AttachmentBinding {
        attachment_id: descriptor.id.clone(),
        label: label.to_owned(),
        span: TextSpan::from(start..start + label.len()),
    }
}

fn review_skill(text: &str) -> SkillInvocation {
    let start = text.find("$review").expect("the Skill stands in the text");
    SkillInvocation {
        skill_id: SkillId::new("review"),
        name: "review".to_owned(),
        scope: None,
        span: TextSpan::from(start..start + "$review".len()),
    }
}

/// Enters a Session whose one user Message reads `text` with the given
/// bindings, and whose snapshot describes `described`.
fn session_with_message(
    application: &mut Application,
    text: &str,
    skill_invocations: Vec<SkillInvocation>,
    attachments: Vec<AttachmentBinding>,
    described: Vec<AttachmentDescriptor>,
) -> SessionSnapshot {
    let workspace = workspace_dir();
    let (_, mut snapshot) = enter_session(application, workspace.path());
    let prompt = &mut snapshot.prompts[0];
    prompt.text = text.to_owned();
    prompt.skill_invocations.clone_from(&skill_invocations);
    prompt.attachments.clone_from(&attachments);
    let message = snapshot
        .messages
        .iter_mut()
        .find(|message| message.role == MessageRole::User)
        .expect("the fixture carries a user Message");
    message.content = text.to_owned();
    message.skill_invocations = skill_invocations;
    message.attachments = attachments;
    snapshot.attachments = described;
    snapshot.revision = SessionRevision(2);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .expect("deliver the Session's snapshot");
    snapshot
}

fn update(
    application: &mut Application,
    session_id: SessionId,
    revision: SessionRevision,
    changes: Vec<SessionChange>,
) {
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision,
                changes,
            },
        )))
        .expect("apply the Session update");
}

/// The rows of the user Message block whose first row reads `first`: every
/// row down to the last that opens with the block's gutter.
fn message_block(buffer: &Buffer, first: &str) -> (u16, Vec<String>) {
    let rows = buffer_rows(buffer);
    let start = rows
        .iter()
        .position(|row| row.contains(&format!("┃ {first}")))
        .unwrap_or_else(|| panic!("the Message is drawn: {rows:#?}"));
    let gutter = rows[start]
        .find('┃')
        .expect("the block opens with its gutter");
    let block = rows[start..]
        .iter()
        .take_while(|row| row.get(gutter..).is_some_and(|rest| rest.starts_with('┃')))
        .map(|row| row.trim_end().to_owned())
        .collect();
    (u16::try_from(start).expect("row fits the terminal"), block)
}

/// Every cell's style along the first run of `needle` in row `y`.
fn styles_of(buffer: &Buffer, y: u16, needle: &str) -> Vec<Style> {
    let row = &buffer_rows(buffer)[usize::from(y)];
    let offset = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} in {row:?}"));
    let x = u16::try_from(row[..offset].chars().count()).expect("column fits");
    (x..x + u16::try_from(needle.chars().count()).expect("width fits"))
        .map(|x| buffer.cell((x, y)).expect("cell inside the buffer").style())
        .collect()
}

#[test]
fn a_message_without_attachments_renders_as_it_always_has() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let text = "Compare the screenshot with the diagram using $review";
    session_with_message(
        &mut application,
        text,
        vec![review_skill(text)],
        Vec::new(),
        Vec::new(),
    );

    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&buffer, "Compare");
    assert_eq!(
        block,
        vec![
            " ┃ Compare the screenshot with",
            " ┃ the diagram using $review",
        ],
        "{:#?}",
        buffer_rows(&buffer)
    );
    let rows = buffer_rows(&buffer);
    assert_eq!(rows[usize::from(start) + block.len()].trim(), "");
    assert_eq!(
        rows[usize::from(start) + block.len() + 1].trim(),
        "Error: No Agent is selected"
    );
}

#[test]
fn a_user_messages_attachments_are_listed_dimmed_beneath_its_text_inside_the_gutter() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let text = "Compare [Image 1] with [Image 2] using $review";
    session_with_message(
        &mut application,
        text,
        vec![review_skill(text)],
        vec![
            bound(&screenshot(), text, "[Image 1]"),
            bound(&diagram(), text, "[Image 2]"),
        ],
        vec![diagram(), screenshot()],
    );

    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&buffer, "Compare");
    assert_eq!(
        block,
        vec![
            " ┃ Compare [Image 1] with",
            " ┃ [Image 2] using $review",
            " ┃ Image 1 · PNG · 1280×720 ·",
            " ┃ 312 KiB",
            " ┃ Image 2 · GIF · 320×200 ·",
            " ┃ 4.5 KiB",
        ],
        "one line per Attachment in label order, wrapped to the text's width: {:#?}",
        buffer_rows(&buffer)
    );
    assert_eq!(
        buffer_rows(&buffer)[usize::from(start) + block.len()].trim(),
        "",
        "the block ends where its last line does"
    );

    // Both labels are accented as the Skill Invocation beside them is.
    let skill = styles_of(&buffer, start + 1, "$review");
    assert!(skill.iter().all(|style| style.fg == Some(Color::Cyan)));
    for label in ["[Image 1]", "[Image 2]"] {
        let row = (start..)
            .find(|y| buffer_rows(&buffer)[usize::from(*y)].contains(label))
            .expect("the label is drawn");
        assert_eq!(
            styles_of(&buffer, row, label),
            vec![skill[0]; label.len()],
            "{label} is accented as the Skill Invocation is"
        );
    }

    // Each line beneath is dimmed over the block's surface, gutter and all.
    let text_style = styles_of(&buffer, start, "Compare")[0];
    for (y, line) in [
        (start + 2, "Image 1 · PNG · 1280×720 ·"),
        (start + 3, "312 KiB"),
        (start + 4, "Image 2 · GIF · 320×200 ·"),
        (start + 5, "4.5 KiB"),
    ] {
        assert!(
            styles_of(&buffer, y, line)
                .iter()
                .all(|style| style.fg == Some(Color::DarkGray) && style.bg == text_style.bg),
            "{line:?} is dimmed"
        );
        assert_eq!(
            styles_of(&buffer, y, "┃"),
            styles_of(&buffer, start, "┃"),
            "{line:?} stands in the same gutter"
        );
    }
}

#[test]
fn a_label_reads_alone_until_the_session_describes_its_attachment() {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    let text = "What is [Image 1]?";
    let snapshot = session_with_message(
        &mut application,
        text,
        Vec::new(),
        vec![bound(&screenshot(), text, "[Image 1]")],
        Vec::new(),
    );

    let (_, block) = message_block(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "What is",
    );
    assert_eq!(block, vec![" ┃ What is [Image 1]?", " ┃ [Image 1]"]);

    update(
        &mut application,
        snapshot.session.id,
        SessionRevision(snapshot.revision.0 + 1),
        vec![SessionChange::AttachmentsDescribed {
            attachments: vec![screenshot()],
        }],
    );
    let (_, block) = message_block(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "What is",
    );
    assert_eq!(
        block,
        vec![
            " ┃ What is [Image 1]?",
            " ┃ Image 1 · PNG · 1280×720 ·",
            " ┃ 312 KiB",
        ],
        "the Message re-renders once its Attachment is described"
    );

    // Described again, as a client may be told, nothing moves.
    update(
        &mut application,
        snapshot.session.id,
        SessionRevision(snapshot.revision.0 + 2),
        vec![SessionChange::AttachmentsDescribed {
            attachments: vec![screenshot()],
        }],
    );
    let (_, again) = message_block(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "What is",
    );
    assert_eq!(again, block);
}

#[test]
fn a_prompt_awaiting_delivery_lists_its_attachments_as_its_message_will() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    let (session_id, snapshot, _) = enter_active_session(&mut application, workspace.path());
    let text = "Also [Image 1]";
    update(
        &mut application,
        session_id,
        SessionRevision(snapshot.revision.0 + 1),
        vec![
            SessionChange::AttachmentsDescribed {
                attachments: vec![diagram()],
            },
            SessionChange::PromptAdded {
                prompt: Prompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: vec![bound(&diagram(), text, "[Image 1]")],
                    delivery: PromptDelivery::Steer,
                    admission_order: PromptOrder(3),
                    status: PromptStatus::Pending,
                    withdrawal: None,
                    taken: None,
                    author: None,
                },
            },
        ],
    );

    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let (start, block) = message_block(&buffer, "Also");
    assert_eq!(
        block,
        vec![
            " ┃ Also [Image 1]",
            " ┃ Image 1 · GIF · 320×200 ·",
            " ┃ 4.5 KiB"
        ],
        "{:#?}",
        buffer_rows(&buffer)
    );
    assert!(
        styles_of(&buffer, start + 1, "Image 1 · GIF")
            .iter()
            .all(|style| style.fg == Some(Color::DarkGray))
    );
}

/// A PNG as far as its header, which is all the Server reads of one.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(13_u32.to_be_bytes());
    bytes.extend(b"IHDR");
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// What a client that uploaded nothing draws of the Session's user Message
/// reading `first`, from the Session's snapshot alone.
fn drawn_from(snapshot: SessionSnapshot, first: &str) -> Vec<String> {
    let mut application = Application::new(workspace_dir().path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Session");
    message_block(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        first,
    )
    .1
}

/// The Session as the Server holds it once its failed first Turn has settled.
async fn settled(client: &ManagedClient, session_id: SessionId) -> SessionSnapshot {
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read the Session");
            if snapshot.session.working_since.is_none() {
                return snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Session settles")
}

#[tokio::test]
async fn every_client_draws_the_same_rows_from_the_snapshot_even_after_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = workspace_dir();
    let channel = "attachment-transcript-test";
    let config = ServerConfig::new(state_dir.path(), channel)
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let client_config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_data_dir(data_dir.path());
    let server = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server");
    let client = ManagedClient::connect(client_config.clone())
        .await
        .expect("connect managed client");
    let wide = client
        .upload_attachment(png(1280, 720))
        .await
        .expect("upload the first image");
    let tall = client
        .upload_attachment(png(90, 1600))
        .await
        .expect("upload the second image");
    let text = "Compare [Image 1] with [Image 2]";
    let session_id = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: vec![
                    bound(&wide, text, "[Image 1]"),
                    bound(&tall, text, "[Image 2]"),
                ],
            },
        })
        .await
        .expect("create the Session")
        .session
        .id;
    let expected = vec![
        " ┃ Compare [Image 1] with",
        " ┃ [Image 2]",
        " ┃ Image 1 · PNG · 1280×720 ·",
        " ┃ 33 B",
        " ┃ Image 2 · PNG · 90×1600 ·",
        " ┃ 33 B",
    ];

    let second = drawn_from(settled(&client, session_id).await, "Compare");
    assert_eq!(second, expected);
    server.shutdown().await.expect("stop the server");
    drop(client);

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn the replacement server");
    let reconnected = ManagedClient::connect(client_config)
        .await
        .expect("reconnect managed client");
    let restored = reconnected
        .read_session(session_id)
        .await
        .expect("read the restored Session");
    assert_eq!(drawn_from(restored, "Compare"), second);

    drop(reconnected);
    replacement
        .shutdown()
        .await
        .expect("stop the replacement server");
}
