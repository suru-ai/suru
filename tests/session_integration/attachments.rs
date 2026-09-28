//! Attachments: uploading an image's bytes, binding it to a label in a
//! Prompt, and fetching the bytes back, all over the Server's HTTP API.

use crate::server_support::{
    PROGRESS_DEADLINE,
    attachments::{bound, gif, jpeg, png, uploaded, webp},
};
use crate::{
    failing_provider_support::{FailingProviderRuntime, spawn_with_failing_provider},
    provider_support::ControlledProvider,
    support::{
        controlled_selection, create_session, read_session_at_least_revision, read_session_until,
        receive_managed_client_initial_state,
    },
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::{
    StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};
use std::sync::Arc;
use suru::{
    protocol::{
        AdmitPromptRequest, AgentId, AgentIdentity, AttachmentBinding, AttachmentDescriptor,
        AttachmentId, AttachmentKind, CreateSessionRequest, InitialPrompt, Message, MessageRole,
        Prompt, PromptDelivery, PromptId, PromptStatus, RuntimeDescriptor,
        SESSION_ERROR_CODE_HEADER, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SessionChange,
        SessionError, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot, SessionUpdate,
        TextSpan,
    },
    provider::{ProviderAttachment, ProviderEvent},
    server::{self, ManualClock, ServerClock, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

const MEBIBYTE: usize = 1024 * 1024;
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);

fn content_hash(bytes: &[u8]) -> AttachmentId {
    AttachmentId::new(blake3::hash(bytes).to_hex().to_string())
}

async fn upload(
    descriptor: &RuntimeDescriptor,
    declared_type: &str,
    bytes: Vec<u8>,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/attachments", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .header(CONTENT_TYPE, declared_type)
        .body(bytes)
        .send()
        .await
        .expect("send upload")
}

async fn fetch(descriptor: &RuntimeDescriptor, id: &AttachmentId) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{}/v1/attachments/{id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send fetch")
}

async fn head(descriptor: &RuntimeDescriptor, id: &AttachmentId) -> reqwest::Response {
    reqwest::Client::new()
        .head(format!("{}/v1/attachments/{id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send HEAD")
}

async fn fetched(descriptor: &RuntimeDescriptor, id: &AttachmentId) -> (String, Vec<u8>) {
    let response = fetch(descriptor, id)
        .await
        .error_for_status()
        .expect("fetch succeeds");
    let mime_type = response
        .headers()
        .get(CONTENT_TYPE)
        .expect("fetched bytes name their type")
        .to_str()
        .expect("type is text")
        .to_owned();
    let bytes = response.bytes().await.expect("read fetched bytes").to_vec();
    (mime_type, bytes)
}

async fn refusal(response: reqwest::Response) -> (StatusCode, SessionError) {
    let status = response.status();
    let error = response
        .json::<SessionError>()
        .await
        .expect("decode the refusal");
    (status, error)
}

fn creation(
    workspace: &std::path::Path,
    text: &str,
    attachments: Vec<AttachmentBinding>,
) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: None,
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
            attachments,
        },
    }
}

async fn admit(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    text: &str,
    attachments: Vec<AttachmentBinding>,
    delivery: PromptDelivery,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments,
            },
            delivery,
        })
        .send()
        .await
        .expect("send Prompt admission")
}

async fn admitted(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    text: &str,
    attachments: Vec<AttachmentBinding>,
    delivery: PromptDelivery,
) -> Prompt {
    admit(descriptor, session_id, text, attachments, delivery)
        .await
        .error_for_status()
        .expect("Prompt admission succeeds")
        .json()
        .await
        .expect("decode admitted Prompt")
}

fn user_message<'a>(snapshot: &'a SessionSnapshot, content: &str) -> Option<&'a Message> {
    snapshot
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User && message.content == content)
}

/// A second client's view of the Session: its stream, opened on a client of
/// its own, answering with the snapshot it leads with.
async fn watch_session(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> (SessionSnapshot, impl Stream<Item = SessionUpdate> + Unpin) {
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{session_id}/events",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Session stream")
        .error_for_status()
        .expect("the Session stream authenticates");
    let mut events = Box::pin(response.bytes_stream().eventsource());
    let snapshot = loop {
        let event = timeout(PROGRESS_DEADLINE, events.next())
            .await
            .expect("the Session snapshot arrives")
            .expect("the Session stream stays open")
            .expect("the Session stream stays readable");
        if event.event == SESSION_SNAPSHOT_EVENT {
            break serde_json::from_str::<SessionSnapshot>(&event.data)
                .expect("decode the Session snapshot");
        }
    };
    let updates = events.filter_map(|event| async move {
        let event = event.expect("the Session stream stays readable");
        (event.event == SESSION_UPDATED_EVENT).then(|| {
            serde_json::from_str::<SessionUpdate>(&event.data).expect("decode a Session update")
        })
    });
    (snapshot, Box::pin(updates))
}

/// Reads the stream until a change it carries satisfies `found`, answering
/// with that change.
async fn next_change(
    updates: &mut (impl Stream<Item = SessionUpdate> + Unpin),
    described: &str,
    found: impl Fn(&SessionChange) -> bool,
) -> SessionChange {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let update = updates.next().await.expect("the Session stream stays open");
            if let Some(change) = update.changes.into_iter().find(|change| found(change)) {
                return change;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Session stream carries {described}"))
}

#[tokio::test]
async fn each_admitted_format_is_described_by_what_its_bytes_are_whatever_was_declared() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-formats-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    for (bytes, mime_type, width, height) in [
        (png(640, 480), "image/png", 640, 480),
        (jpeg(1920, 1080), "image/jpeg", 1920, 1080),
        (gif(320, 200), "image/gif", 320, 200),
        (webp(800, 600), "image/webp", 800, 600),
    ] {
        // Every upload claims to be a PDF; only its bytes say what it is.
        let response = upload(&descriptor, "application/pdf", bytes.clone()).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{mime_type}");
        assert_eq!(
            response
                .json::<AttachmentDescriptor>()
                .await
                .expect("decode Attachment descriptor"),
            AttachmentDescriptor {
                id: content_hash(&bytes),
                kind: AttachmentKind::Image { width, height },
                mime_type: mime_type.to_owned(),
                byte_length: bytes.len() as u64,
            }
        );
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_upload_of_another_format_or_over_five_mebibytes_is_refused_with_a_reason() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-refusal-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let bitmap = b"BM\x3a\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0\x01\0\0\0\x01\0\0\0".to_vec();
    let (status, error) = refusal(upload(&descriptor, "image/png", bitmap.clone()).await).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        error,
        SessionError {
            code: SessionErrorCode::UnsupportedAttachment,
            message: "Only PNG, JPEG, GIF, and WebP images can be attached".to_owned(),
        }
    );

    let mut oversized = png(8000, 6000);
    oversized.resize(5 * MEBIBYTE + 1, 0);
    let (status, error) = refusal(upload(&descriptor, "image/png", oversized.clone()).await).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error,
        SessionError {
            code: SessionErrorCode::AttachmentTooLarge,
            message: "An image may be at most 5 MiB, and this one is 5.1 MiB".to_owned(),
        }
    );

    for refused in [&bitmap, &oversized] {
        assert_eq!(
            fetch(&descriptor, &content_hash(refused)).await.status(),
            StatusCode::NOT_FOUND,
            "a refused upload is not stored"
        );
    }
    let mut at_the_cap = png(8000, 6000);
    at_the_cap.resize(5 * MEBIBYTE, 0);
    assert_eq!(
        upload(&descriptor, "image/png", at_the_cap).await.status(),
        StatusCode::CREATED,
        "an image of exactly five mebibytes is stored"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_same_bytes_uploaded_twice_are_one_attachment() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-dedup-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let bytes = gif(16, 9);

    let first = upload(&descriptor, "image/gif", bytes.clone()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = first.json::<AttachmentDescriptor>().await.unwrap();
    let second = upload(&descriptor, "image/png", bytes.clone()).await;
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "the second upload finds the bytes already stored"
    );
    assert_eq!(second.json::<AttachmentDescriptor>().await.unwrap(), first);
    assert_eq!(
        fetched(&descriptor, &first.id).await,
        ("image/gif".to_owned(), bytes)
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_attachment_is_fetched_by_id_with_its_sniffed_type_and_an_unknown_id_is_not_found() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-fetch-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let bytes = webp(1200, 900);
    let attachment = uploaded(&descriptor, bytes.clone()).await;

    assert_eq!(
        fetched(&descriptor, &attachment.id).await,
        ("image/webp".to_owned(), bytes)
    );
    let (status, error) = refusal(fetch(&descriptor, &content_hash(b"never uploaded")).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error.code, SessionErrorCode::AttachmentNotFound);
    let unauthenticated = reqwest::Client::new()
        .get(format!(
            "{}/v1/attachments/{}",
            descriptor.base_url, attachment.id
        ))
        .send()
        .await
        .expect("send unauthenticated fetch");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    server.shutdown().await.expect("shut down server");
}

/// Whether an Attachment is stored is asked with a `HEAD` of its fetch
/// route, which answers the type and length a fetch would without the bytes.
#[tokio::test]
async fn a_head_of_an_attachment_answers_whether_it_is_stored_without_its_bytes() {
    use suru::{
        managed_client::{ManagedClient, ManagedClientConfig},
        protocol::Outlook,
    };

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-head-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let bytes = webp(1200, 900);
    let attachment = uploaded(&descriptor, bytes.clone()).await;

    let stored = head(&descriptor, &attachment.id).await;
    assert_eq!(stored.status(), StatusCode::OK);
    assert_eq!(stored.headers()[CONTENT_TYPE], "image/webp");
    assert_eq!(
        stored.headers()[CONTENT_LENGTH],
        bytes.len().to_string().as_str()
    );
    assert!(
        stored.headers().get(SESSION_ERROR_CODE_HEADER).is_none(),
        "a stored Attachment is no error"
    );
    assert!(
        stored
            .bytes()
            .await
            .expect("read the HEAD answer")
            .is_empty(),
        "a HEAD answer carries no bytes"
    );
    let unknown = content_hash(b"never uploaded");
    let missing = head(&descriptor, &unknown).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        missing.headers()[SESSION_ERROR_CODE_HEADER],
        "attachment_not_found",
        "a body-less Not Found still says why"
    );
    assert!(
        missing
            .bytes()
            .await
            .expect("read the HEAD answer")
            .is_empty()
    );
    // A fetch's refusal names its code in the header as in its body.
    assert_eq!(
        fetch(&descriptor, &unknown).await.headers()[SESSION_ERROR_CODE_HEADER],
        "attachment_not_found"
    );
    let unauthenticated = reqwest::Client::new()
        .head(format!(
            "{}/v1/attachments/{}",
            descriptor.base_url, attachment.id
        ))
        .send()
        .await
        .expect("send unauthenticated HEAD");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // The managed client asks the same way, of its own Server or through an
    // Outlook.
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "attachment-head-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    assert!(
        client
            .attachment_exists(&attachment.id)
            .await
            .expect("ask after a stored Attachment")
    );
    assert!(
        !client
            .attachment_exists(&unknown)
            .await
            .expect("ask after an unknown Attachment")
    );
    assert!(
        client
            .outlook(Outlook::Local)
            .attachment_exists(&attachment.id)
            .await
            .expect("ask through the Outlook")
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn turn_beginning_and_steer_prompts_carry_their_bindings_to_every_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "attachment-binding-test").expect("configure server"),
        runtime.clone(),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let screenshot = uploaded(&descriptor, png(640, 480)).await;
    let diagram = uploaded(&descriptor, gif(320, 200)).await;

    // A Prompt whose text is only a label is admitted, and begins the Turn.
    let first = vec![bound(&screenshot, "[Image 1]", "[Image 1]")];
    let created = create_session(
        &descriptor,
        &creation(workspace.path(), "[Image 1]", first.clone()),
    )
    .await;
    let session_id = created.session.id;
    assert_eq!(created.prompts[0].attachments, first);
    let (watched, mut updates) = watch_session(&descriptor, session_id).await;
    assert_eq!(
        watched.prompts[0].attachments, first,
        "a second client's snapshot carries the binding"
    );

    let mut provider_session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-attachments", "high", "fast"),
        });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    let begun = read_session_until(
        &client,
        &descriptor,
        session_id,
        "the first user Message",
        |snapshot| user_message(snapshot, "[Image 1]").is_some(),
    )
    .await;
    assert_eq!(
        user_message(&begun, "[Image 1]").unwrap().attachments,
        first
    );

    // A steer binds both, one of them for the second time.
    let steer_text = "Now compare [Image 1] with [Image 2]";
    let steered = vec![
        bound(&screenshot, steer_text, "[Image 1]"),
        bound(&diagram, steer_text, "[Image 2]"),
    ];
    let steer = admitted(
        &descriptor,
        session_id,
        steer_text,
        steered.clone(),
        PromptDelivery::Steer,
    )
    .await;
    assert_eq!(steer.attachments, steered);
    let SessionChange::PromptAdded { prompt } = next_change(
        &mut updates,
        "the steer Prompt",
        |change| matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == steer.id),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(prompt.attachments, steered);
    timeout(PROGRESS_DEADLINE, provider_session.next_steer())
        .await
        .expect("the steer reaches the Provider")
        .succeed();
    let SessionChange::MessageAdded { message } =
        next_change(&mut updates, "the steer's user Message", |change| {
            matches!(change, SessionChange::MessageAdded { message } if message.content == steer_text)
        })
        .await
    else {
        unreachable!()
    };
    assert_eq!(message.attachments, steered);

    // Once the Turn settles, a queued Prompt begins a Turn of its own.
    provider_session.emit(ProviderEvent::TurnCompleted);
    read_session_until(&client, &descriptor, session_id, "idle", |snapshot| {
        snapshot.session.working_since.is_none()
    })
    .await;
    let queued_text = "Once more: [Image 2]";
    let queued = vec![bound(&diagram, queued_text, "[Image 2]")];
    admitted(
        &descriptor,
        session_id,
        queued_text,
        queued.clone(),
        PromptDelivery::Queue,
    )
    .await;
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the queued Turn reaches the Provider")
        .succeed();

    let snapshot = read_session_until(
        &client,
        &descriptor,
        session_id,
        "three user Messages",
        |snapshot| user_message(snapshot, queued_text).is_some(),
    )
    .await;
    for (text, bindings) in [
        ("[Image 1]", &first),
        (steer_text, &steered),
        (queued_text, &queued),
    ] {
        let prompt = snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.text == text)
            .expect("the Prompt stands");
        assert_eq!(&prompt.attachments, bindings, "{text}");
        assert_eq!(
            &user_message(&snapshot, text)
                .expect("the user Message stands")
                .attachments,
            bindings,
            "{text}"
        );
    }
    let (rewatched, _) = watch_session(&descriptor, session_id).await;
    assert_eq!(
        rewatched.prompts, snapshot.prompts,
        "a client arriving later sees every binding"
    );
    assert_eq!(rewatched.messages, snapshot.messages);

    provider_session.emit(ProviderEvent::TurnCompleted);
    drop(provider_session);
    server.shutdown().await.expect("shut down server");
}

fn delivered(label: &str, mime_type: &str, bytes: &[u8]) -> ProviderAttachment {
    ProviderAttachment {
        label: label.to_owned(),
        mime_type: mime_type.to_owned(),
        bytes: bytes.to_vec(),
    }
}

#[tokio::test]
async fn the_provider_receives_each_attachments_bytes_in_label_order_on_turn_start_and_steer() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "attachment-delivery-test").expect("configure server"),
        runtime.clone(),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let screenshot_bytes = png(640, 480);
    let photo_bytes = jpeg(1920, 1080);
    let diagram_bytes = gif(320, 200);
    let screenshot = uploaded(&descriptor, screenshot_bytes.clone()).await;
    let photo = uploaded(&descriptor, photo_bytes.clone()).await;
    let diagram = uploaded(&descriptor, diagram_bytes.clone()).await;

    // Bound out of label order: the Provider is handed them in the order
    // their labels stand in the text, which keeps its labels.
    let initial_text = "Compare [Image 1] with [Image 2]";
    let created = create_session(
        &descriptor,
        &creation(
            workspace.path(),
            initial_text,
            vec![
                bound(&photo, initial_text, "[Image 2]"),
                bound(&screenshot, initial_text, "[Image 1]"),
            ],
        ),
    )
    .await;
    let mut provider_session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-attachments", "high", "fast"),
        });
    let turn = timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the Turn reaches the Provider");
    assert_eq!(turn.prompt(), initial_text);
    assert_eq!(
        turn.attachments(),
        [
            delivered("[Image 1]", "image/png", &screenshot_bytes),
            delivered("[Image 2]", "image/jpeg", &photo_bytes),
        ]
    );
    turn.succeed();

    // A label standing twice for one Attachment reaches the Provider once,
    // where it first stands.
    let steer_text = "See [Image 3] beside [Image 1], and [Image 1] again";
    let steer_bindings = {
        let second = steer_text
            .rfind("[Image 1]")
            .expect("the label stands twice");
        vec![
            bound(&screenshot, steer_text, "[Image 1]"),
            AttachmentBinding {
                attachment_id: screenshot.id.clone(),
                label: "[Image 1]".to_owned(),
                span: TextSpan::from(second..second + "[Image 1]".len()),
            },
            bound(&diagram, steer_text, "[Image 3]"),
        ]
    };
    admitted(
        &descriptor,
        created.session.id,
        steer_text,
        steer_bindings,
        PromptDelivery::Steer,
    )
    .await;
    let steer = timeout(PROGRESS_DEADLINE, provider_session.next_steer())
        .await
        .expect("the steer reaches the Provider");
    assert_eq!(steer.prompt(), steer_text);
    assert_eq!(
        steer.attachments(),
        [
            delivered("[Image 3]", "image/gif", &diagram_bytes),
            delivered("[Image 1]", "image/png", &screenshot_bytes),
        ]
    );
    steer.succeed();

    provider_session.emit(ProviderEvent::TurnCompleted);
    drop(provider_session);
    server.shutdown().await.expect("shut down server");
}

/// A Prompt recalled from composer history is sent again with the very
/// bindings it was first sent with: it is admitted, and its Attachments reach
/// the Provider again, having been sent once spending nothing.
#[tokio::test]
async fn a_prompt_resent_with_the_same_bindings_hands_the_provider_its_attachments_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "attachment-resend-test").expect("configure server"),
        runtime.clone(),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let bytes = png(640, 480);
    let screenshot = uploaded(&descriptor, bytes.clone()).await;
    let text = "See [Image 1]";
    let bindings = vec![bound(&screenshot, text, "[Image 1]")];

    let created = create_session(
        &descriptor,
        &creation(workspace.path(), text, bindings.clone()),
    )
    .await;
    let session_id = created.session.id;
    let mut provider_session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-attachments", "high", "fast"),
        });
    let first = timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the first Turn reaches the Provider");
    assert_eq!(
        first.attachments(),
        [delivered("[Image 1]", "image/png", &bytes)]
    );
    first.succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    read_session_until(&client, &descriptor, session_id, "idle", |snapshot| {
        snapshot.session.working_since.is_none()
    })
    .await;

    let resent = admitted(
        &descriptor,
        session_id,
        text,
        bindings.clone(),
        PromptDelivery::Queue,
    )
    .await;
    assert_eq!(resent.attachments, bindings);
    let again = timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("the resent Turn reaches the Provider");
    assert_eq!(again.prompt(), text);
    assert_eq!(
        again.attachments(),
        [delivered("[Image 1]", "image/png", &bytes)]
    );
    again.succeed();

    provider_session.emit(ProviderEvent::TurnCompleted);
    drop(provider_session);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn prompts_whose_attachment_bindings_cannot_stand_are_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-admission-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let screenshot = uploaded(&descriptor, png(640, 480)).await;
    let created = create_session(
        &descriptor,
        &creation(workspace.path(), "Begin plainly", Vec::new()),
    )
    .await;
    let session_id = created.session.id;
    read_session_at_least_revision(
        &reqwest::Client::new(),
        &descriptor,
        session_id,
        SessionRevision(2),
    )
    .await;

    let text = "Look at [Image 1]";
    let unknown = AttachmentBinding {
        attachment_id: content_hash(b"never uploaded"),
        ..bound(&screenshot, text, "[Image 1]")
    };
    let outside = AttachmentBinding {
        span: TextSpan { start: 8, end: 40 },
        ..bound(&screenshot, text, "[Image 1]")
    };
    let mismatched = AttachmentBinding {
        label: "[Image 2]".to_owned(),
        ..bound(&screenshot, text, "[Image 1]")
    };
    let crowded = "[Image 1]".repeat(11);
    let eleven = (0..11)
        .map(|index| AttachmentBinding {
            span: TextSpan {
                start: index * 9,
                end: index * 9 + 9,
            },
            ..bound(&screenshot, &crowded, "[Image 1]")
        })
        .collect::<Vec<_>>();
    for (text, bindings, code) in [
        (
            text,
            vec![unknown.clone()],
            SessionErrorCode::AttachmentNotFound,
        ),
        (
            text,
            vec![outside],
            SessionErrorCode::InvalidAttachmentBinding,
        ),
        (
            text,
            vec![mismatched],
            SessionErrorCode::InvalidAttachmentBinding,
        ),
        (
            crowded.as_str(),
            eleven.clone(),
            SessionErrorCode::TooManyAttachments,
        ),
    ] {
        let (status, error) = refusal(
            admit(
                &descriptor,
                session_id,
                text,
                bindings,
                PromptDelivery::Queue,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{code:?}");
        assert_eq!(error.code, code, "{}", error.message);
        assert!(!error.message.is_empty());
    }
    let (status, error) = refusal(
        reqwest::Client::new()
            .post(format!("{}/v1/sessions", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&creation(workspace.path(), text, vec![unknown]))
            .send()
            .await
            .expect("send Session creation"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error.code, SessionErrorCode::AttachmentNotFound);

    assert_eq!(
        crate::support::read_session(&descriptor, session_id)
            .await
            .prompts
            .len(),
        1,
        "no refused Prompt was admitted"
    );
    let ten = admitted(
        &descriptor,
        session_id,
        &crowded,
        eleven[..10].to_vec(),
        PromptDelivery::Queue,
    )
    .await;
    assert_eq!(ten.attachments, eleven[..10]);

    // A retry is the same Prompt only with the same bindings.
    let retry = |attachments: Vec<AttachmentBinding>| {
        reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{session_id}/prompts",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .json(&AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: ten.id,
                    text: crowded.clone(),
                    skill_invocations: Vec::new(),
                    attachments,
                },
                delivery: PromptDelivery::Queue,
            })
            .send()
    };
    let repeated = retry(eleven[..10].to_vec())
        .await
        .expect("send exact retry");
    assert_eq!(repeated.status(), StatusCode::OK);
    let repeated = repeated.json::<Prompt>().await.unwrap();
    assert_eq!(
        (repeated.id, repeated.attachments),
        (ten.id, ten.attachments.clone()),
        "an exact retry answers with the Prompt already admitted"
    );
    let (status, error) = refusal(
        retry(eleven[..9].to_vec())
            .await
            .expect("send conflicting retry"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error.code, SessionErrorCode::PromptConflict);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn bindings_and_bytes_survive_a_server_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "attachment-restart-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let bytes = jpeg(1024, 768);
    let photo = uploaded(original.descriptor(), bytes.clone()).await;
    let text = "What is in [Image 1]?";
    let bindings = vec![bound(&photo, text, "[Image 1]")];
    let created = create_session(
        original.descriptor(),
        &creation(workspace.path(), text, bindings.clone()),
    )
    .await;
    let before_restart = read_session_at_least_revision(
        &reqwest::Client::new(),
        original.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;
    assert_eq!(
        user_message(&before_restart, text)
            .expect("the failed Turn keeps its user Message")
            .attachments,
        bindings
    );
    original.shutdown().await.expect("stop original server");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    let restored = crate::support::read_session(replacement.descriptor(), created.session.id).await;
    assert_eq!(restored, before_restart);
    assert_eq!(restored.prompts[0].attachments, bindings);
    assert_eq!(
        fetched(replacement.descriptor(), &photo.id).await,
        ("image/jpeg".to_owned(), bytes)
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

/// Descriptors in the order a snapshot carries them: by id.
fn by_id(mut descriptors: Vec<AttachmentDescriptor>) -> Vec<AttachmentDescriptor> {
    descriptors.sort_by(|left, right| left.id.cmp(&right.id));
    descriptors
}

#[tokio::test]
async fn every_client_describes_the_attachments_a_session_binds_even_after_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "attachment-descriptor-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let descriptor = original.descriptor().clone();
    let screenshot = uploaded(&descriptor, png(1280, 720)).await;
    let diagram = uploaded(&descriptor, gif(320, 200)).await;
    let photo = uploaded(&descriptor, webp(64, 48)).await;
    let unbound = uploaded(&descriptor, jpeg(8, 8)).await;

    // The creating client learns both from the snapshot it is answered with.
    let text = "Compare [Image 1] with [Image 2]";
    let created = create_session(
        &descriptor,
        &creation(
            workspace.path(),
            text,
            vec![
                bound(&screenshot, text, "[Image 1]"),
                bound(&diagram, text, "[Image 2]"),
            ],
        ),
    )
    .await;
    let session_id = created.session.id;
    let both = by_id(vec![screenshot.clone(), diagram.clone()]);
    assert_eq!(created.attachments, both);

    // So does a second client, from the snapshot its stream leads with.
    let (watched, mut updates) = watch_session(&descriptor, session_id).await;
    assert_eq!(watched.attachments, both);
    let settled = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        session_id,
        "settled",
        |snapshot| snapshot.session.working_since.is_none(),
    )
    .await;
    assert_eq!(settled.attachments, both);

    // A Prompt binding one Attachment already described and one not yet is
    // described ahead of its addition, and only by what the Session lacked.
    let again = "Now [Image 1] beside [Image 2]";
    let queued = admitted(
        &descriptor,
        session_id,
        again,
        vec![
            bound(&screenshot, again, "[Image 1]"),
            bound(&photo, again, "[Image 2]"),
        ],
        PromptDelivery::Queue,
    )
    .await;
    let update = timeout(PROGRESS_DEADLINE, async {
        loop {
            let update = updates.next().await.expect("the Session stream stays open");
            if update.changes.iter().any(|change| {
                matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == queued.id)
            }) {
                return update;
            }
        }
    })
    .await
    .expect("the Session stream carries the queued Prompt");
    let described = update
        .changes
        .iter()
        .position(|change| {
            change
                == &SessionChange::AttachmentsDescribed {
                    attachments: vec![photo.clone()],
                }
        })
        .unwrap_or_else(|| panic!("the new Attachment is described: {:#?}", update.changes));
    let added = update
        .changes
        .iter()
        .position(|change| matches!(change, SessionChange::PromptAdded { .. }))
        .expect("the Prompt is added");
    assert!(
        described < added,
        "the description precedes the Prompt binding it"
    );
    let all = by_id(vec![screenshot.clone(), diagram.clone(), photo.clone()]);
    let before_restart = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        session_id,
        "the queued Prompt's user Message",
        |snapshot| {
            user_message(snapshot, again).is_some() && snapshot.session.working_since.is_none()
        },
    )
    .await;
    assert_eq!(before_restart.attachments, all);
    assert!(
        !before_restart.attachments.contains(&unbound),
        "an upload no Prompt binds describes nothing"
    );

    // A Session binding nothing carries nothing.
    let plain = create_session(
        &descriptor,
        &creation(workspace.path(), "Nothing attached", Vec::new()),
    )
    .await;
    assert!(plain.attachments.is_empty());
    original.shutdown().await.expect("stop original server");

    // A client connecting after a restart reads them from the snapshot alone.
    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    let restored = crate::support::read_session(replacement.descriptor(), session_id).await;
    assert_eq!(restored.attachments, all);
    assert_eq!(restored, before_restart);
    let (rewatched, _) = watch_session(replacement.descriptor(), session_id).await;
    assert_eq!(rewatched.attachments, all);
    assert!(
        crate::support::read_session(replacement.descriptor(), plain.session.id)
            .await
            .attachments
            .is_empty()
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

/// A server on the given timings, whose Provider fails every Turn so each
/// Session settles as soon as its Prompt is delivered.
async fn spawn_with_timings(
    state_dir: &std::path::Path,
    channel: &str,
    timings: ServerTimings,
) -> server::RunningServer {
    server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir, channel).expect("configure server"),
        Arc::new(FailingProviderRuntime),
        timings,
    )
    .await
    .expect("spawn server")
}

/// A server measuring an hour's Attachment grace by a clock the test moves.
async fn spawn_with_manual_clock(
    state_dir: &std::path::Path,
    channel: &str,
) -> (server::RunningServer, ManualClock) {
    let (clock, hand) = ServerClock::manual();
    let server = spawn_with_timings(
        state_dir,
        channel,
        ServerTimings::default()
            .with_attachment_grace(HOUR)
            .with_clock(clock),
    )
    .await;
    (server, hand)
}

/// Creates a Session whose first Prompt binds `bindings`, and waits for its
/// failed first Turn to leave it deletable.
async fn settled_session(
    descriptor: &RuntimeDescriptor,
    workspace: &std::path::Path,
    text: &str,
    bindings: Vec<AttachmentBinding>,
) -> SessionId {
    let session_id = create_session(descriptor, &creation(workspace, text, bindings))
        .await
        .session
        .id;
    read_session_until(
        &reqwest::Client::new(),
        descriptor,
        session_id,
        "settled",
        |snapshot| snapshot.session.working_since.is_none(),
    )
    .await;
    session_id
}

async fn delete_session(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    let response = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session deletion");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_a_session_past_the_grace_period_removes_the_attachments_only_it_references() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_timings(
        state_dir.path(),
        "attachment-deletion-test",
        ServerTimings::default().with_attachment_grace(Duration::ZERO),
    )
    .await;
    let descriptor = server.descriptor().clone();
    let own = uploaded(&descriptor, png(10, 10)).await;
    let shared = uploaded(&descriptor, gif(20, 20)).await;

    let text = "[Image 1] beside [Image 2]";
    let deleted = settled_session(
        &descriptor,
        workspace.path(),
        text,
        vec![
            bound(&own, text, "[Image 1]"),
            bound(&shared, text, "[Image 2]"),
        ],
    )
    .await;
    let kept = settled_session(
        &descriptor,
        workspace.path(),
        "[Image 1]",
        vec![bound(&shared, "[Image 1]", "[Image 1]")],
    )
    .await;
    delete_session(&descriptor, deleted).await;

    assert_eq!(
        fetch(&descriptor, &own.id).await.status(),
        StatusCode::NOT_FOUND,
        "an Attachment only the deleted Session bound goes with it"
    );
    assert_eq!(
        fetch(&descriptor, &shared.id).await.status(),
        StatusCode::OK,
        "an Attachment another Session binds stays"
    );
    let remaining = crate::support::read_session(&descriptor, kept).await;
    assert_eq!(remaining.prompts[0].attachments[0].attachment_id, shared.id);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn deleting_a_session_within_the_grace_period_leaves_its_attachments_for_the_sweep() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-grace-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let own = uploaded(&descriptor, png(10, 10)).await;
    let shared = uploaded(&descriptor, gif(20, 20)).await;

    let text = "[Image 1] beside [Image 2]";
    let deleted = settled_session(
        &descriptor,
        workspace.path(),
        text,
        vec![
            bound(&own, text, "[Image 1]"),
            bound(&shared, text, "[Image 2]"),
        ],
    )
    .await;
    settled_session(
        &descriptor,
        workspace.path(),
        "[Image 1]",
        vec![bound(&shared, "[Image 1]", "[Image 1]")],
    )
    .await;
    delete_session(&descriptor, deleted).await;

    assert_eq!(
        fetched(&descriptor, &own.id).await,
        ("image/png".to_owned(), png(10, 10)),
        "an Attachment uploaded within the grace period is the sweep's to reclaim"
    );
    assert_eq!(
        fetch(&descriptor, &shared.id).await.status(),
        StatusCode::OK,
        "an Attachment another Session binds stays"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn uploading_an_attachment_again_restarts_its_grace_period() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, clock) =
        spawn_with_manual_clock(state_dir.path(), "attachment-reupload-test").await;
    let descriptor = server.descriptor().clone();
    let again = uploaded(&descriptor, png(10, 10)).await;
    let once = uploaded(&descriptor, gif(20, 20)).await;
    let text = "[Image 1] beside [Image 2]";
    let session_id = settled_session(
        &descriptor,
        workspace.path(),
        text,
        vec![
            bound(&again, text, "[Image 1]"),
            bound(&once, text, "[Image 2]"),
        ],
    )
    .await;

    // Both are past their grace period now; only one is uploaded again, as a
    // client binding it anew would.
    clock.advance(HOUR + MINUTE);
    let reuploaded = upload(&descriptor, "image/png", png(10, 10)).await;
    assert_eq!(reuploaded.status(), StatusCode::OK);
    assert_eq!(
        reuploaded
            .json::<AttachmentDescriptor>()
            .await
            .expect("decode Attachment descriptor"),
        again
    );
    delete_session(&descriptor, session_id).await;

    assert_eq!(
        fetch(&descriptor, &again.id).await.status(),
        StatusCode::OK,
        "the Attachment uploaded again is within its grace period"
    );
    assert_eq!(
        fetch(&descriptor, &once.id).await.status(),
        StatusCode::NOT_FOUND,
        "the Attachment not uploaded again is past its grace period"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn admitting_a_prompt_restarts_the_grace_period_of_the_attachments_it_binds() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, clock) =
        spawn_with_manual_clock(state_dir.path(), "attachment-admission-grace-test").await;
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let admitted_again = uploaded(&descriptor, png(10, 10)).await;
    let created_again = uploaded(&descriptor, jpeg(30, 30)).await;
    let once = uploaded(&descriptor, gif(20, 20)).await;
    let text = "[Image 1], [Image 2], and [Image 3]";
    let first = settled_session(
        &descriptor,
        workspace.path(),
        text,
        vec![
            bound(&admitted_again, text, "[Image 1]"),
            bound(&created_again, text, "[Image 2]"),
            bound(&once, text, "[Image 3]"),
        ],
    )
    .await;
    let later = settled_session(&descriptor, workspace.path(), "Begin plainly", Vec::new()).await;

    // Every upload is past its grace period now. Neither Attachment is
    // uploaded again: each is bound as it stands, as a Prompt recalled from
    // history would bind it.
    clock.advance(HOUR + MINUTE);
    let again_text = "Again: [Image 1]";
    let readmitted = admitted(
        &descriptor,
        later,
        again_text,
        vec![bound(&admitted_again, again_text, "[Image 1]")],
        PromptDelivery::Queue,
    )
    .await;
    read_session_until(&client, &descriptor, later, "settled again", |snapshot| {
        snapshot.session.working_since.is_none()
            && snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == readmitted.id && prompt.status != PromptStatus::Pending)
    })
    .await;
    let recreated = settled_session(
        &descriptor,
        workspace.path(),
        "[Image 1]",
        vec![bound(&created_again, "[Image 1]", "[Image 1]")],
    )
    .await;

    delete_session(&descriptor, first).await;
    assert_eq!(
        fetch(&descriptor, &once.id).await.status(),
        StatusCode::NOT_FOUND,
        "an Attachment no admission bound again is past its grace period"
    );
    for kept in [&admitted_again, &created_again] {
        assert_eq!(fetch(&descriptor, &kept.id).await.status(), StatusCode::OK);
    }
    assert_eq!(
        user_message(
            &crate::support::read_session(&descriptor, later).await,
            again_text
        )
        .expect("the admitted Prompt's user Message stands")
        .attachments,
        readmitted.attachments,
        "the later Session still carries its binding"
    );

    // With every Session that binds them gone, their admission alone keeps
    // them: each was stamped referenced when its Prompt was admitted.
    delete_session(&descriptor, later).await;
    delete_session(&descriptor, recreated).await;
    for kept in [&admitted_again, &created_again] {
        assert_eq!(
            fetch(&descriptor, &kept.id).await.status(),
            StatusCode::OK,
            "an Attachment bound within the grace period is the sweep's to reclaim"
        );
    }

    server.shutdown().await.expect("shut down server");
}

/// Waits for a sweep to reclaim `attachment`, which the idle flush ending the
/// writer's next burst of work runs.
async fn wait_until_swept(descriptor: &RuntimeDescriptor, attachment: &AttachmentDescriptor) {
    timeout(PROGRESS_DEADLINE, async {
        while fetch(descriptor, &attachment.id).await.status() != StatusCode::NOT_FOUND {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a sweep reclaims the orphaned Attachment");
}

#[tokio::test]
async fn an_idle_flush_sweeps_uploads_no_prompt_binds_once_past_their_grace_period() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, clock) = spawn_with_manual_clock(state_dir.path(), "attachment-sweep-test").await;
    let descriptor = server.descriptor().clone();
    let orphan = uploaded(&descriptor, png(10, 10)).await;
    clock.advance(2 * MINUTE);
    let young = uploaded(&descriptor, gif(20, 20)).await;

    // The first upload is past its grace period now and the second is not.
    // Neither is bound: its label was deleted, or its Prompt refused.
    clock.advance(HOUR - MINUTE);
    let session_id =
        settled_session(&descriptor, workspace.path(), "Begin plainly", Vec::new()).await;
    wait_until_swept(&descriptor, &orphan).await;
    assert_eq!(
        fetched(&descriptor, &young.id).await,
        ("image/gif".to_owned(), gif(20, 20)),
        "an upload within its grace period survives the sweep"
    );

    let text = "Now [Image 1]";
    let bindings = vec![bound(&young, text, "[Image 1]")];
    let prompt = admitted(
        &descriptor,
        session_id,
        text,
        bindings.clone(),
        PromptDelivery::Queue,
    )
    .await;
    assert_eq!(
        prompt.attachments, bindings,
        "the upload that survived is bound"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sweep_never_reclaims_an_attachment_a_stored_prompt_binds_however_old() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, clock) =
        spawn_with_manual_clock(state_dir.path(), "attachment-sweep-bound-test").await;
    let descriptor = server.descriptor().clone();
    let kept = uploaded(&descriptor, png(10, 10)).await;
    let orphan = uploaded(&descriptor, gif(20, 20)).await;
    let text = "Keep [Image 1]";
    let bindings = vec![bound(&kept, text, "[Image 1]")];
    let session_id = settled_session(&descriptor, workspace.path(), text, bindings.clone()).await;

    clock.advance(1000 * HOUR);
    settled_session(&descriptor, workspace.path(), "Begin plainly", Vec::new()).await;
    wait_until_swept(&descriptor, &orphan).await;

    assert_eq!(
        fetched(&descriptor, &kept.id).await,
        ("image/png".to_owned(), png(10, 10)),
        "an Attachment a stored Prompt binds is never swept"
    );
    let snapshot = crate::support::read_session(&descriptor, session_id).await;
    assert_eq!(snapshot.prompts[0].attachments, bindings);
    assert_eq!(snapshot.attachments, vec![kept]);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_quiet_server_sweeps_uploads_no_prompt_binds_once_a_sweep_interval_passes() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (clock, hand) = ServerClock::manual();
    let server = spawn_with_timings(
        state_dir.path(),
        "attachment-quiet-sweep-test",
        ServerTimings::default()
            .with_attachment_grace(HOUR)
            .with_attachment_sweep_interval(5 * MINUTE)
            .with_clock(clock),
    )
    .await;
    let descriptor = server.descriptor().clone();
    let orphan = uploaded(&descriptor, png(10, 10)).await;
    hand.advance(2 * MINUTE);
    let young = uploaded(&descriptor, gif(20, 20)).await;

    // No Session is created or touched: uploads alone never reach the
    // writer, so only the passing of the sweep interval can sweep. By now the
    // first upload is past its grace period and the second is not.
    hand.advance(HOUR - MINUTE);
    wait_until_swept(&descriptor, &orphan).await;
    assert_eq!(
        fetched(&descriptor, &young.id).await,
        ("image/gif".to_owned(), gif(20, 20)),
        "an upload within its grace period survives the sweep"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn server_start_sweeps_uploads_no_stored_prompt_binds_once_past_their_grace_period() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "attachment-start-sweep-test";
    let timings = ServerTimings::default().with_attachment_grace(HOUR);
    let original = spawn_with_timings(state_dir.path(), channel, timings.clone()).await;
    let descriptor = original.descriptor().clone();
    let orphan = uploaded(&descriptor, png(10, 10)).await;
    let kept = uploaded(&descriptor, gif(20, 20)).await;
    let text = "Keep [Image 1]";
    let bindings = vec![bound(&kept, text, "[Image 1]")];
    let session_id = settled_session(&descriptor, workspace.path(), text, bindings.clone()).await;
    original.shutdown().await.expect("stop original server");

    // The Server comes back after both uploads have passed their grace period.
    let (clock, hand) = ServerClock::manual();
    hand.advance(HOUR + MINUTE);
    let replacement =
        spawn_with_timings(state_dir.path(), channel, timings.with_clock(clock)).await;
    let descriptor = replacement.descriptor().clone();

    assert_eq!(
        fetch(&descriptor, &orphan.id).await.status(),
        StatusCode::NOT_FOUND,
        "the sweep at start reclaims the upload no stored Prompt binds"
    );
    assert_eq!(
        fetched(&descriptor, &kept.id).await,
        ("image/gif".to_owned(), gif(20, 20)),
        "the sweep at start keeps the Attachment a stored Prompt binds"
    );
    let restored = crate::support::read_session(&descriptor, session_id).await;
    assert_eq!(restored.prompts[0].attachments, bindings);

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

/// The composer's own paste, answered by a real upload through the managed
/// client, builds a Prompt the Server admits exactly as it was bound.
#[tokio::test]
async fn a_prompt_the_composer_builds_around_a_pasted_image_is_admitted_as_bound() {
    use suru::{
        managed_client::{ManagedClient, ManagedClientConfig},
        protocol::Outlook,
        tui::{
            Application, ApplicationEvent, ApplicationTransition, ClipboardRead, CommandId,
            SemanticCommandId, TerminalFacts,
        },
    };

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let _server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "attachment-composer-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "attachment-composer-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let mut application = Application::new(workspace.path(), TerminalFacts::default());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Compare ".to_owned(),
        )))
        .expect("type the Prompt");
    let ApplicationTransition::ReadClipboard(paste) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ComposerClipboardPaste,
        )))
        .expect("paste from the clipboard")
    else {
        panic!("a paste reads the clipboard");
    };
    let clipboard_png = png(1280, 720);
    let ApplicationTransition::UploadAttachment {
        paste,
        origin,
        png: pasted,
    } = application
        .handle_event(ApplicationEvent::ClipboardRead {
            paste,
            read: ClipboardRead::Image {
                png: clipboard_png.clone(),
            },
        })
        .expect("answer the clipboard read")
    else {
        panic!("a clipboard image is uploaded");
    };
    assert_eq!(origin, Outlook::Local);
    let stored = client
        .outlook(origin.clone())
        .upload_attachment(pasted)
        .await
        .expect("upload the pasted image");
    assert_eq!(stored.id, content_hash(&clipboard_png));
    assert_eq!(
        stored.kind,
        AttachmentKind::Image {
            width: 1280,
            height: 720
        }
    );
    application
        .handle_event(ApplicationEvent::AttachmentUploaded {
            paste,
            descriptor: stored.clone(),
        })
        .expect("answer the upload");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the Prompt")
    else {
        panic!("the Landing's Prompt begins a Session");
    };
    assert_eq!(request.prompt.text, "Compare [Image 1] ");
    assert_eq!(
        request.prompt.attachments,
        vec![bound(&stored, &request.prompt.text, "[Image 1]")]
    );

    let created = client
        .outlook(origin)
        .create_session(request.clone())
        .await
        .expect("the Server admits the composer's Prompt");
    assert_eq!(created.prompts[0].text, request.prompt.text);
    assert_eq!(created.prompts[0].attachments, request.prompt.attachments);

    assert_eq!(
        client
            .fetch_attachment(&stored.id)
            .await
            .expect("fetch the stored bytes"),
        ("image/png".to_owned(), clipboard_png)
    );

    // A refusal reaches the client as the Server's own words.
    let mut oversized = png(1, 1);
    oversized.resize(5 * MEBIBYTE + 1024, 0);
    let error = client
        .upload_attachment(oversized)
        .await
        .expect_err("an image over the cap is refused");
    let refusal = error
        .downcast_ref::<SessionError>()
        .expect("the refusal is the Server's");
    assert_eq!(refusal.code, SessionErrorCode::AttachmentTooLarge);
    assert_eq!(
        refusal.message,
        "An image may be at most 5 MiB, and this one is 5.1 MiB"
    );
}
