//! Attachments a test places on a Prompt: images whose headers are all the
//! Server reads of them, uploaded over the Server's own route, and bound to
//! the labels a Prompt's text carries.

use reqwest::header::CONTENT_TYPE;
use suru::protocol::{AttachmentBinding, AttachmentDescriptor, RuntimeDescriptor, TextSpan};

pub fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(13_u32.to_be_bytes());
    bytes.extend(b"IHDR");
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

pub fn jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8];
    bytes.extend(height.to_be_bytes());
    bytes.extend(width.to_be_bytes());
    bytes.extend([1, 1, 0x11, 0]);
    bytes.extend([0xFF, 0xD9]);
    bytes
}

pub fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend(width.to_le_bytes());
    bytes.extend(height.to_le_bytes());
    bytes.extend([0, 0, 0, 0x3B]);
    bytes
}

/// An extended WebP, whose canvas stores each dimension less one.
pub fn webp(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
    bytes.extend(&(width - 1).to_le_bytes()[..3]);
    bytes.extend(&(height - 1).to_le_bytes()[..3]);
    bytes
}

/// Uploads `bytes` to the Server `descriptor` names, answering with the
/// Attachment it stored them as.
pub async fn uploaded(descriptor: &RuntimeDescriptor, bytes: Vec<u8>) -> AttachmentDescriptor {
    reqwest::Client::new()
        .post(format!("{}/v1/attachments", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .expect("send upload")
        .error_for_status()
        .expect("upload succeeds")
        .json()
        .await
        .expect("decode Attachment descriptor")
}

/// Binds `label`, found in `text`, to the uploaded Attachment.
pub fn bound(attachment: &AttachmentDescriptor, text: &str, label: &str) -> AttachmentBinding {
    let start = text.find(label).expect("label stands in the text");
    AttachmentBinding {
        attachment_id: attachment.id.clone(),
        label: label.to_owned(),
        span: TextSpan::from(start..start + label.len()),
    }
}
