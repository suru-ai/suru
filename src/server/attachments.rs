//! The Attachment routes: uploading an image's bytes, fetching them back by
//! id, and refusing a Prompt whose bindings cannot stand (ADR 0037).

use axum::{
    Json,
    body::to_bytes,
    extract::{Path as AxumPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};

use super::{AppState, is_authenticated, session_error_response};
use crate::{
    attachments::{
        BindingRefusal, PromptAttachmentError, UPLOAD_BODY_LIMIT, UploadError, UploadRefusal,
    },
    protocol::{AttachmentId, InitialPrompt, SessionErrorCode},
};

/// Stores the request body's bytes as an Attachment and answers with its
/// descriptor. Only this route reads a body past the ordinary command cap.
/// The body's declared content type is advisory and goes unread: the format
/// is sniffed from the bytes themselves.
pub(super) async fn upload_attachment(State(state): State<AppState>, request: Request) -> Response {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(bytes) = to_bytes(request.into_body(), UPLOAD_BODY_LIMIT).await else {
        return upload_refusal_response(&UploadRefusal::TooLarge { byte_length: None });
    };
    match state.attachments.upload(Vec::from(bytes)).await {
        Ok(uploaded) => (
            if uploaded.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(uploaded.descriptor),
        )
            .into_response(),
        Err(UploadError::Refused(refusal)) => upload_refusal_response(&refusal),
        Err(UploadError::Storage(error)) => {
            tracing::warn!("Attachment could not be stored: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn upload_refusal_response(refusal: &UploadRefusal) -> Response {
    let (status, code) = match refusal {
        UploadRefusal::TooLarge { .. } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            SessionErrorCode::AttachmentTooLarge,
        ),
        UploadRefusal::Unsupported(_) => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            SessionErrorCode::UnsupportedAttachment,
        ),
    };
    session_error_response(status, code, refusal.message())
}

/// Serves a stored Attachment's bytes under the type they were sniffed as.
pub(super) async fn fetch_attachment(
    State(state): State<AppState>,
    AxumPath(attachment_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let attachment_id = AttachmentId::new(attachment_id);
    match state.attachments.fetch(attachment_id.clone()).await {
        Ok(Some((mime_type, bytes))) => {
            let Ok(content_type) = HeaderValue::from_str(&mime_type) else {
                tracing::warn!("Attachment {attachment_id} is stored with an unusable type");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            ([(CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Ok(None) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::AttachmentNotFound,
            format!("Attachment {attachment_id} is not stored on this Server"),
        ),
        Err(error) => {
            tracing::warn!("Attachment could not be read: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Refuses a Prompt whose Attachment bindings cannot stand: too many of them,
/// a label its text does not carry where it is bound, or an Attachment this
/// Server has not stored. Reads only, so it may run before any other work.
// A rejection is the Response the handler returns as-is, which is the axum
// idiom; boxing it would only add an allocation to every refusal.
#[allow(clippy::result_large_err)]
pub(super) async fn check_prompt_attachments(
    state: &AppState,
    prompt: &InitialPrompt,
) -> Result<(), Response> {
    binding_response(
        state
            .attachments
            .check_prompt(&prompt.text, &prompt.attachments)
            .await,
    )
}

/// Refuses a Prompt as [`check_prompt_attachments`] does, and otherwise
/// stamps every Attachment it binds as referenced now. Runs right before the
/// Prompt is recorded, after every await of its admission, so no Session's
/// deletion can reclaim a bound Attachment before the flush that joins it.
#[allow(clippy::result_large_err)]
pub(super) async fn reference_prompt_attachments(
    state: &AppState,
    prompt: &InitialPrompt,
) -> Result<(), Response> {
    binding_response(
        state
            .attachments
            .reference_prompt(&prompt.text, &prompt.attachments)
            .await,
    )
}

#[allow(clippy::result_large_err)]
fn binding_response(checked: Result<(), PromptAttachmentError>) -> Result<(), Response> {
    match checked {
        Ok(()) => Ok(()),
        Err(PromptAttachmentError::Refused(refusal)) => {
            let code = match refusal {
                BindingRefusal::TooMany { .. } => SessionErrorCode::TooManyAttachments,
                BindingRefusal::Invalid(_) => SessionErrorCode::InvalidAttachmentBinding,
                BindingRefusal::Unknown(_) => SessionErrorCode::AttachmentNotFound,
            };
            Err(session_error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                code,
                refusal.message(),
            ))
        }
        Err(PromptAttachmentError::Storage(error)) => {
            tracing::warn!("Prompt Attachments could not be checked: {error}");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}
