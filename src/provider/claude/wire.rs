//! The serde types Suru exchanges with a Claude Code CLI over its stream-json wire.
//!
//! Everything below is verified against Claude Code CLI 2.1.237 and the Agent SDK type definitions
//! 0.3.241 — the wire is an SDK implementation detail rather than a documented surface, so drift is
//! ours to absorb (ADR 0010). Decoding tolerates fields and control-response subtypes it does not
//! know, because the CLI grows both freely.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The envelope a control request travels in, correlated by its `request_id`.
#[derive(Serialize)]
pub(super) struct ControlRequestEnvelope<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) request_id: &'a str,
    pub(super) request: &'a ControlRequest,
}

impl<'a> ControlRequestEnvelope<'a> {
    pub(super) fn new(request_id: &'a str, request: &'a ControlRequest) -> Self {
        Self {
            kind: "control_request",
            request_id,
            request,
        }
    }
}

/// A control request Suru issues to the CLI.
#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(super) enum ControlRequest {
    ListModels,
}

/// One newline-delimited message the CLI wrote, decoded only as far as routing needs: everything
/// that is not a control response is conversation, which lands with Sessions in a later slice.
/// The response itself stays undecoded here so a subtype this build does not know can be told
/// apart from a malformed one it does.
#[derive(Deserialize)]
pub(super) struct IncomingMessage {
    #[serde(rename = "type")]
    pub(super) kind: String,
    pub(super) response: Option<Value>,
}

/// The CLI's answer to one control request, correlated back by `request_id`.
#[derive(Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(super) enum ControlResponse {
    Success {
        request_id: String,
        #[serde(default)]
        response: Option<Value>,
    },
    Error {
        request_id: String,
        error: String,
    },
}

/// What `list_models` answers with: the rows the CLI's own model picker offers.
#[derive(Deserialize)]
pub(super) struct NativeModelList {
    pub(super) models: Vec<NativeModel>,
}

/// One row of the CLI's model picker. `value` is what a spawn's model flag accepts — an alias such
/// as `sonnet` or `opus[1m]` — so it is the Model ID Suru presents; the row's canonical
/// `resolvedModel` is not decoded because Suru presents rows verbatim rather than collapsing
/// aliases.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeModel {
    pub(super) value: String,
    pub(super) display_name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) supported_effort_levels: Vec<String>,
}
