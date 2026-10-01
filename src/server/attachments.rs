//! The Attachment routes: uploading an image's bytes, fetching them back by
//! id, answering whether they are stored without sending them, and the answer
//! refusing a Prompt whose bindings cannot stand (ADR 0037).

use axum::{
    Json,
    body::to_bytes,
    extract::{Path as AxumPath, Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};

use super::{AppState, is_authenticated, session_error_response};
use crate::{
    attachments::{BindingRefusal, UPLOAD_BODY_LIMIT, UploadError, UploadRefusal},
    protocol::{AttachmentId, SessionErrorCode},
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
        Ok(None) => attachment_not_found(&attachment_id),
        Err(error) => {
            tracing::warn!("Attachment could not be read: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Answers whether an Attachment is stored, with the type and length its
/// fetch would answer, without reading or sending its bytes: what a client
/// asks before offering a recalled Prompt's Attachments again.
pub(super) async fn head_attachment(
    State(state): State<AppState>,
    AxumPath(attachment_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let attachment_id = AttachmentId::new(attachment_id);
    match state
        .attachments
        .type_and_length(attachment_id.clone())
        .await
    {
        Ok(Some((mime_type, byte_length))) => {
            let Ok(content_type) = HeaderValue::from_str(&mime_type) else {
                tracing::warn!("Attachment {attachment_id} is stored with an unusable type");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            (
                [
                    (CONTENT_TYPE, content_type),
                    (CONTENT_LENGTH, HeaderValue::from(byte_length)),
                ],
                (),
            )
                .into_response()
        }
        Ok(None) => attachment_not_found(&attachment_id),
        Err(error) => {
            tracing::warn!("Attachment could not be read: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn attachment_not_found(attachment_id: &AttachmentId) -> Response {
    session_error_response(
        StatusCode::NOT_FOUND,
        SessionErrorCode::AttachmentNotFound,
        format!("Attachment {attachment_id} is not stored on this Server"),
    )
}

/// A Prompt whose Attachment bindings cannot stand, as the Session API
/// refuses one: too many of them, a label its text does not carry where it is
/// bound, or an Attachment this Server has not stored.
pub(super) fn binding_refusal_response(refusal: &BindingRefusal) -> Response {
    let code = match refusal {
        BindingRefusal::TooMany { .. } => SessionErrorCode::TooManyAttachments,
        BindingRefusal::Invalid(_) => SessionErrorCode::InvalidAttachmentBinding,
        BindingRefusal::Unknown(_) => SessionErrorCode::AttachmentNotFound,
    };
    session_error_response(StatusCode::UNPROCESSABLE_ENTITY, code, refusal.message())
}
